//! This module allows us to move failed CFDs to a separate table so
//! that we can reason about loading them independently from open
//! CFDs.
//!
//! Failed CFDs are ones that did not reach the point of having a DLC
//! on chain. This can happen either because the taker's order was
//! rejected or contract setup did not finish successfully.
//!
//! Therefore, it also provides an interface to load failed CFDs: the
//! `FailedCfdAggregate` trait. Implementers of the trait will be able
//! to call the `crate::db::load_all_cfds` API, which loads all types
//! of CFD.

use crate::db;
use crate::db::delete_from_cfds_table;
use crate::db::delete_from_events_table;
use crate::db::event_log::EventLog;
use crate::db::event_log::EventLogEntry;
use crate::db::load_cfd_events;
use crate::db::load_cfd_row;
use crate::db::CfdAggregate;
use crate::db::Connection;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use model::impl_sqlx_type_display_from_str;
use model::long_and_short_leverage;
use model::Contracts;
use model::EventKind;
use model::FeeAccount;
use model::Fees;
use model::FundingFee;
use model::Identity;
use model::Leverage;
use model::OrderId;
use model::Position;
use model::Price;
use model::Role;
use model::Timestamp;
use sqlx::pool::PoolConnection;
use sqlx::Connection as _;
use sqlx::Sqlite;
use sqlx::Transaction;
use std::fmt;
use std::str;

/// A trait for building an aggregate based on a `FailedCfd`.
pub trait FailedCfdAggregate: CfdAggregate {
    fn new_failed(args: Self::CtorArgs, cfd: FailedCfd) -> Self;
}

/// Data loaded from the database about a failed CFD.
#[derive(Debug, Clone, Copy)]
pub struct FailedCfd {
    pub id: OrderId,
    pub position: Position,
    pub initial_price: Price,
    pub taker_leverage: Leverage,
    pub n_contracts: Contracts,
    pub counterparty_network_identity: Identity,
    pub role: Role,
    pub fees: Fees,
    pub kind: Kind,
    pub creation_timestamp: Timestamp,
}

/// The type of failed CFD.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    OfferRejected,
    ContractSetupFailed,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::OfferRejected => "OfferRejected",
            Kind::ContractSetupFailed => "ContractSetupFailed",
        };

        s.fmt(f)
    }
}

impl str::FromStr for Kind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let kind = match s {
            "OfferRejected" => Kind::OfferRejected,
            "ContractSetupFailed" => Kind::ContractSetupFailed,
            other => bail!("Not a failed CFD Kind: {other}"),
        };

        Ok(kind)
    }
}

impl_sqlx_type_display_from_str!(Kind);

impl Connection {
    pub async fn move_to_failed_cfds(&self) -> Result<()> {
        let ids = self.failed_cfd_ids_according_to_events().await?;

        if !ids.is_empty() {
            tracing::debug!("Moving ContractSetupFailed CFDs to failed_cfds table: {ids:?}");
        }

        for id in ids.into_iter() {
            let pool = self.inner.clone();

            let fut = async move {
                let mut conn = pool.acquire().await?;
                let mut db_tx = conn.begin().await?;

                let cfd = load_cfd_row(&mut db_tx, id).await?;

                let events = load_cfd_events(&mut db_tx, id, 0).await?;
                let event_log = EventLog::new(&events);

                insert_failed_cfd(&mut db_tx, cfd, &event_log).await?;
                insert_event_log(&mut db_tx, id, event_log).await?;

                delete_from_events_table(&mut db_tx, id).await?;
                delete_from_cfds_table(&mut db_tx, id).await?;

                db_tx.commit().await?;

                anyhow::Ok(())
            };

            match fut.await {
                Ok(()) => tracing::debug!(order_id = %id, "Moved CFD to `failed_cfds` table"),
                Err(e) => tracing::warn!(order_id = %id, "Failed to move failed CFD: {e:#}"),
            }
        }

        Ok(())
    }

    /// Load a failed CFD from the database.
    pub(super) async fn load_failed_cfd<C>(&self, id: OrderId, args: C::CtorArgs) -> Result<C>
    where
        C: FailedCfdAggregate,
    {
        let mut conn = self.inner.acquire().await?;

        let cfd = sqlx::query!(
            r#"
            SELECT
                uuid as "id: model::OrderId",
                position as "position: model::Position",
                initial_price as "initial_price: model::Price",
                taker_leverage as "taker_leverage: model::Leverage",
                n_contracts as "n_contracts: model::Contracts",
                counterparty_network_identity as "counterparty_network_identity: model::Identity",
                role as "role: model::Role",
                fees as "fees: model::Fees",
                kind as "kind: Kind"
            FROM
                failed_cfds
            WHERE
                failed_cfds.uuid = $1
            "#,
            id
        )
        .fetch_one(&mut conn)
        .await?;

        let creation_timestamp = load_creation_timestamp(&mut conn, id).await?;

        let cfd = FailedCfd {
            id,
            position: cfd.position,
            initial_price: cfd.initial_price,
            taker_leverage: cfd.taker_leverage,
            n_contracts: cfd.n_contracts,
            counterparty_network_identity: cfd.counterparty_network_identity,
            role: cfd.role,
            fees: cfd.fees,
            kind: cfd.kind,
            creation_timestamp,
        };

        Ok(C::new_failed(args, cfd))
    }
}

async fn insert_failed_cfd(
    conn: &mut Transaction<'_, Sqlite>,
    cfd: db::Cfd,
    event_log: &EventLog,
) -> Result<()> {
    let kind = if event_log.contains(&EventKind::OfferRejected) {
        Kind::OfferRejected
    } else if event_log.contains(&EventKind::ContractSetupFailed) {
        Kind::ContractSetupFailed
    } else {
        bail!("Failed CFD does not have expected event")
    };

    let n_contracts = cfd
        .quantity_usd
        .try_into_u64()
        .expect("number of contracts to fit into a u64");
    let n_contracts = Contracts::new(n_contracts);

    let fees = {
        let (long_leverage, short_leverage) =
            long_and_short_leverage(cfd.taker_leverage, cfd.role, cfd.position);

        let initial_funding_fee = FundingFee::calculate(
            cfd.initial_price,
            cfd.quantity_usd,
            long_leverage,
            short_leverage,
            cfd.initial_funding_rate,
            cfd.settlement_interval.whole_hours(),
        )
        .expect("values from db to be sane");

        let fee_account = FeeAccount::new(cfd.position, cfd.role)
            .add_opening_fee(cfd.opening_fee)
            .add_funding_fee(initial_funding_fee);

        Fees::new(fee_account.balance())
    };

    let query_result = sqlx::query!(
        r#"
        INSERT INTO failed_cfds
        (
            uuid,
            position,
            initial_price,
            taker_leverage,
            n_contracts,
            counterparty_network_identity,
            role,
            fees,
            kind
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        cfd.id,
        cfd.position,
        cfd.initial_price,
        cfd.taker_leverage,
        n_contracts,
        cfd.counterparty_network_identity,
        cfd.role,
        fees,
        kind,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert into failed_cfds");
    }

    Ok(())
}

async fn insert_event_log(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    event_log: EventLog,
) -> Result<()> {
    for EventLogEntry { name, created_at } in event_log.0.iter() {
        let query_result = sqlx::query!(
            r#"
            INSERT INTO event_log_failed (
                cfd_id,
                name,
                created_at
            )
            VALUES
            (
                (SELECT id FROM failed_cfds WHERE failed_cfds.uuid = $1),
                $2, $3
            )
            "#,
            id,
            name,
            created_at
        )
        .execute(&mut *conn)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert into event_log_failed");
        }
    }

    Ok(())
}

/// Obtain the time at which the failed CFD was created, according to
/// the `event_log_failed` table.
///
/// A CFD can fail either because:
///
/// - the offer was rejected; or
/// - contract setup finished unsuccessfully.
///
/// Therefore we can take the creation timestamp of either the
/// `EventKind::OfferRejected` variant or the
/// `EventKind::ContractSetupFailed` variant.
///
/// We choose to not depend on `EventKind::ContractSetupStarted`
/// because it's impossible to automatically ensure that we will
/// always emit such an event before emitting the
/// `EventKind::ContractSetupFailed`. The slight inaccuracy is
/// preferred over the possibility of running into bugs.
async fn load_creation_timestamp(
    conn: &mut PoolConnection<Sqlite>,
    id: OrderId,
) -> Result<Timestamp> {
    let row = sqlx::query!(
        r#"
        SELECT
            event_log_failed.created_at
        FROM
            event_log_failed
        JOIN
            failed_cfds on failed_cfds.id = event_log_failed.cfd_id
        WHERE
            failed_cfds.uuid = $1 AND (event_log_failed.name = $2 OR event_log_failed.name = $3)
        "#,
        id,
        model::EventKind::OFFER_REJECTED,
        model::EventKind::CONTRACT_SETUP_FAILED,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(Timestamp::new(row.created_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory;
    use crate::db::tests::dummy_cfd;
    use crate::db::tests::lock_confirmed;
    use crate::db::tests::order_rejected;
    use crate::db::tests::setup_failed;
    use model::CfdEvent;

    #[tokio::test]
    async fn given_offer_rejected_when_move_cfds_to_failed_table_then_can_load_cfd_as_failed() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(order_rejected(&cfd)).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_failed = db.load_failed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_err());
        assert!(load_from_events.is_empty());
        assert!(load_from_failed.is_ok());
    }

    #[tokio::test]
    async fn given_contract_setup_failed_when_move_cfds_to_failed_table_then_can_load_cfd_as_failed(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(setup_failed(&cfd)).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_failed = db.load_failed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_err());
        assert!(load_from_events.is_empty());
        assert!(load_from_failed.is_ok());
    }

    #[tokio::test]
    async fn given_cfd_without_failed_events_when_move_cfds_to_failed_table_then_cannot_load_cfd_as_failed(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        // appending an event which does not imply that the CFD failed
        db.append_event(lock_confirmed(&cfd)).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_failed = db.load_failed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_ok());
        assert_eq!(load_from_events.len(), 1);
        assert!(load_from_failed.is_err());
    }

    #[tokio::test]
    async fn given_contract_setup_failed_when_move_cfds_to_failed_table_then_projection_aggregate_stays_the_same(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(setup_failed(&cfd)).await.unwrap();

        let projection_open = {
            let projection_open = db
                .load_open_cfd::<crate::projection::Cfd>(order_id, bdk::bitcoin::Network::Testnet)
                .await
                .unwrap();
            projection_open.with_current_quote(None) // unconditional processing in `projection`
        };

        db.move_to_failed_cfds().await.unwrap();

        let projection_failed = {
            let projection_failed = db
                .load_failed_cfd::<crate::projection::Cfd>(order_id, bdk::bitcoin::Network::Testnet)
                .await
                .unwrap();
            projection_failed.with_current_quote(None) // unconditional processing in `projection`
        };

        // this comparison actually omits the `aggregated` field on
        // `projection::Cfd` because it is not used when aggregating
        // from a failed CFD
        assert_eq!(projection_open, projection_failed);
    }

    #[tokio::test]
    async fn given_order_rejected_when_move_cfds_to_failed_table_then_projection_aggregate_stays_the_same(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(order_rejected(&cfd)).await.unwrap();

        let projection_open = {
            let projection_open = db
                .load_open_cfd::<crate::projection::Cfd>(order_id, bdk::bitcoin::Network::Testnet)
                .await
                .unwrap();
            projection_open.with_current_quote(None) // unconditional processing in `projection`
        };

        db.move_to_failed_cfds().await.unwrap();

        let projection_failed = {
            let projection_failed = db
                .load_failed_cfd::<crate::projection::Cfd>(order_id, bdk::bitcoin::Network::Testnet)
                .await
                .unwrap();
            projection_failed.with_current_quote(None) // unconditional processing in `projection`
        };

        // this comparison actually omits the `aggregated` field on
        // `projection::Cfd` because it is not used when aggregating
        // from a failed CFD
        assert_eq!(projection_open, projection_failed);
    }

    #[derive(Debug, Clone)]
    struct DummyAggregate;

    impl CfdAggregate for DummyAggregate {
        type CtorArgs = ();

        fn new(_: Self::CtorArgs, _: crate::db::Cfd) -> Self {
            Self
        }

        fn apply(self, _: CfdEvent) -> Self {
            Self
        }

        fn version(&self) -> u32 {
            0
        }
    }

    impl FailedCfdAggregate for DummyAggregate {
        fn new_failed(_: Self::CtorArgs, _: FailedCfd) -> Self {
            Self
        }
    }
}
