use crate::model::cfd::{Cfd, CfdState, Order, OrderId, Origin};
use crate::model::{BitMexPriceEventId, Leverage, Position};
use anyhow::{Context, Result};
use rocket_db_pools::sqlx;
use sqlx::pool::PoolConnection;
use sqlx::{Acquire, Sqlite, SqlitePool};
use std::convert::TryInto;
use std::mem;

pub async fn run_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

pub async fn insert_order(order: &Order, conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<()> {
    let uuid = serde_json::to_string(&order.id).unwrap();
    let trading_pair = serde_json::to_string(&order.trading_pair).unwrap();
    let position = serde_json::to_string(&order.position).unwrap();
    let initial_price = serde_json::to_string(&order.price).unwrap();
    let min_quantity = serde_json::to_string(&order.min_quantity).unwrap();
    let max_quantity = serde_json::to_string(&order.max_quantity).unwrap();
    let leverage = order.leverage.0;
    let liquidation_price = serde_json::to_string(&order.liquidation_price).unwrap();
    let creation_timestamp = serde_json::to_string(&order.creation_timestamp).unwrap();
    let term = serde_json::to_string(&order.term).unwrap();
    let origin = serde_json::to_string(&order.origin).unwrap();
    let oracle_event_id = order.oracle_event_id.to_string();

    sqlx::query!(
        r#"
            insert into orders (
                uuid,
                trading_pair,
                position,
                initial_price,
                min_quantity,
                max_quantity,
                leverage,
                liquidation_price,
                creation_timestamp,
                term,
                origin,
                oracle_event_id
            ) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
            "#,
        uuid,
        trading_pair,
        position,
        initial_price,
        min_quantity,
        max_quantity,
        leverage,
        liquidation_price,
        creation_timestamp,
        term,
        origin,
        oracle_event_id
    )
    .execute(conn)
    .await?;

    Ok(())
}

pub async fn load_order_by_id(
    id: OrderId,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<Order> {
    let uuid = serde_json::to_string(&id).unwrap();

    let row = sqlx::query!(
        r#"
        select * from orders where uuid = ?;
        "#,
        uuid
    )
    .fetch_one(conn)
    .await?;

    let uuid = serde_json::from_str(row.uuid.as_str()).unwrap();
    let trading_pair = serde_json::from_str(row.trading_pair.as_str()).unwrap();
    let position = serde_json::from_str(row.position.as_str()).unwrap();
    let initial_price = serde_json::from_str(row.initial_price.as_str()).unwrap();
    let min_quantity = serde_json::from_str(row.min_quantity.as_str()).unwrap();
    let max_quantity = serde_json::from_str(row.max_quantity.as_str()).unwrap();
    let leverage = Leverage(row.leverage.try_into().unwrap());
    let liquidation_price = serde_json::from_str(row.liquidation_price.as_str()).unwrap();
    let creation_timestamp = serde_json::from_str(row.creation_timestamp.as_str()).unwrap();
    let term = serde_json::from_str(row.term.as_str()).unwrap();
    let origin = serde_json::from_str(row.origin.as_str()).unwrap();

    Ok(Order {
        id: uuid,
        trading_pair,
        position,
        price: initial_price,
        min_quantity,
        max_quantity,
        leverage,
        liquidation_price,
        creation_timestamp,
        term,
        origin,
        oracle_event_id: row.oracle_event_id.parse().unwrap(),
    })
}

pub async fn insert_cfd(cfd: Cfd, conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<()> {
    let mut tx = conn.begin().await?;

    let order_uuid = serde_json::to_string(&cfd.order.id)?;
    let order_row = sqlx::query!(
        r#"
        select * from orders where uuid = ?;
        "#,
        order_uuid
    )
    .fetch_one(&mut tx)
    .await?;

    let order_id = order_row.id;
    let quantity_usd = serde_json::to_string(&cfd.quantity_usd)?;

    let cfd_state = serde_json::to_string(&cfd.state)?;

    // save cfd + state in a transaction to make sure the state is only inserted if the cfd was
    // inserted

    let cfd_id = sqlx::query!(
        r#"
            insert into cfds (
                order_id,
                order_uuid,
                quantity_usd
            ) values (?, ?, ?);
            "#,
        order_id,
        order_uuid,
        quantity_usd,
    )
    .execute(&mut tx)
    .await?
    .last_insert_rowid();

    sqlx::query!(
        r#"
            insert into cfd_states (
                cfd_id,
                state
            ) values (?, ?);
            "#,
        cfd_id,
        cfd_state,
    )
    .execute(&mut tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn insert_new_cfd_state_by_order_id(
    order_id: OrderId,
    new_state: &CfdState,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<()> {
    let cfd_id = load_cfd_id_by_order_uuid(order_id, conn).await?;
    let latest_cfd_state_in_db = load_latest_cfd_state(cfd_id, conn)
        .await
        .context("loading latest state failed")?;

    if mem::discriminant(&latest_cfd_state_in_db) == mem::discriminant(new_state) {
        // Since we have states where we add information this happens quite frequently
        tracing::trace!(
            "Same state transition for cfd with order_id {}: {}",
            order_id,
            latest_cfd_state_in_db
        );
    }

    let cfd_state = serde_json::to_string(new_state)?;

    sqlx::query!(
        r#"
        insert into cfd_states (
            cfd_id,
            state
        ) values (?, ?);
        "#,
        cfd_id,
        cfd_state,
    )
    .execute(conn)
    .await?;

    Ok(())
}

#[allow(dead_code)]
async fn load_cfd_id_by_order_uuid(
    order_uuid: OrderId,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<i64> {
    let order_uuid = serde_json::to_string(&order_uuid)?;

    let cfd_id = sqlx::query!(
        r#"
        select
            id
        from cfds
        where order_uuid = ?;
        "#,
        order_uuid
    )
    .fetch_one(conn)
    .await?;

    let cfd_id = cfd_id.id.context("No cfd found")?;

    Ok(cfd_id)
}

#[allow(dead_code)]
async fn load_latest_cfd_state(
    cfd_id: i64,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<CfdState> {
    let latest_cfd_state = sqlx::query!(
        r#"
        select
            state
        from cfd_states
        where cfd_id = ?
        order by id desc
        limit 1;
        "#,
        cfd_id
    )
    .fetch_one(conn)
    .await?;

    let latest_cfd_state_in_db: CfdState = serde_json::from_str(latest_cfd_state.state.as_str())?;

    Ok(latest_cfd_state_in_db)
}

pub async fn load_cfd_by_order_id(
    order_id: OrderId,
    conn: &mut PoolConnection<Sqlite>,
) -> Result<Cfd> {
    let order_uuid = serde_json::to_string(&order_id)?;

    let row = sqlx::query!(
        r#"
        select
            orders.uuid as order_id,
            orders.initial_price as price,
            orders.min_quantity as min_quantity,
            orders.max_quantity as max_quantity,
            orders.leverage as leverage,
            orders.trading_pair as trading_pair,
            orders.position as position,
            orders.origin as origin,
            orders.liquidation_price as liquidation_price,
            orders.creation_timestamp as creation_timestamp,
            orders.term as term,
            orders.oracle_event_id,
            cfds.quantity_usd as quantity_usd,
            cfd_states.state as state
        from cfds as cfds
        inner join orders as orders on cfds.order_id = orders.id
        inner join cfd_states as cfd_states on cfd_states.cfd_id = cfds.id
        where cfd_states.state in (
            select
              state
              from cfd_states
            where cfd_id = cfds.id
            order by id desc
            limit 1
        )
        and orders.uuid = ?
        "#,
        order_uuid
    )
    .fetch_one(conn)
    .await?;

    let order_id = serde_json::from_str(row.order_id.as_str()).unwrap();
    let trading_pair = serde_json::from_str(row.trading_pair.as_str()).unwrap();
    let position: Position = serde_json::from_str(row.position.as_str()).unwrap();
    let price = serde_json::from_str(row.price.as_str()).unwrap();
    let min_quantity = serde_json::from_str(row.min_quantity.as_str()).unwrap();
    let max_quantity = serde_json::from_str(row.max_quantity.as_str()).unwrap();
    let leverage = Leverage(row.leverage.try_into().unwrap());
    let liquidation_price = serde_json::from_str(row.liquidation_price.as_str()).unwrap();
    let creation_timestamp = serde_json::from_str(row.creation_timestamp.as_str()).unwrap();
    let term = serde_json::from_str(row.term.as_str()).unwrap();
    let origin: Origin = serde_json::from_str(row.origin.as_str()).unwrap();
    let oracle_event_id = row.oracle_event_id.parse().unwrap();

    let quantity = serde_json::from_str(row.quantity_usd.as_str()).unwrap();
    let latest_state = serde_json::from_str(row.state.as_str()).unwrap();

    let order = Order {
        id: order_id,
        trading_pair,
        position,
        price,
        min_quantity,
        max_quantity,
        leverage,
        liquidation_price,
        creation_timestamp,
        term,
        origin,
        oracle_event_id,
    };

    Ok(Cfd {
        order,
        quantity_usd: quantity,
        state: latest_state,
    })
}

/// Loads all CFDs with the latest state as the CFD state
pub async fn load_all_cfds(conn: &mut PoolConnection<Sqlite>) -> anyhow::Result<Vec<Cfd>> {
    // TODO: Could be optimized with something like but not sure it's worth the complexity:

    let rows = sqlx::query!(
        r#"
        select
            orders.uuid as order_id,
            orders.initial_price as price,
            orders.min_quantity as min_quantity,
            orders.max_quantity as max_quantity,
            orders.leverage as leverage,
            orders.trading_pair as trading_pair,
            orders.position as position,
            orders.origin as origin,
            orders.liquidation_price as liquidation_price,
            orders.creation_timestamp as creation_timestamp,
            orders.term as term,
            orders.oracle_event_id,
            cfds.quantity_usd as quantity_usd,
            cfd_states.state as state
        from cfds as cfds
        inner join orders as orders on cfds.order_id = orders.id
        inner join cfd_states as cfd_states on cfd_states.cfd_id = cfds.id
        where cfd_states.state in (
            select
              state
              from cfd_states
            where cfd_id = cfds.id
            order by id desc
            limit 1
        )
        "#
    )
    .fetch_all(conn)
    .await?;

    let cfds = rows
        .iter()
        .map(|row| {
            let order_id = serde_json::from_str(row.order_id.as_str()).unwrap();
            let trading_pair = serde_json::from_str(row.trading_pair.as_str()).unwrap();
            let position: Position = serde_json::from_str(row.position.as_str()).unwrap();
            let price = serde_json::from_str(row.price.as_str()).unwrap();
            let min_quantity = serde_json::from_str(row.min_quantity.as_str()).unwrap();
            let max_quantity = serde_json::from_str(row.max_quantity.as_str()).unwrap();
            let leverage = Leverage(row.leverage.try_into().unwrap());
            let liquidation_price = serde_json::from_str(row.liquidation_price.as_str()).unwrap();
            let creation_timestamp = serde_json::from_str(row.creation_timestamp.as_str()).unwrap();
            let term = serde_json::from_str(row.term.as_str()).unwrap();
            let origin: Origin = serde_json::from_str(row.origin.as_str()).unwrap();
            let oracle_event_id = row.oracle_event_id.clone().parse().unwrap();

            let quantity = serde_json::from_str(row.quantity_usd.as_str()).unwrap();
            let latest_state = serde_json::from_str(row.state.as_str()).unwrap();

            let order = Order {
                id: order_id,
                trading_pair,
                position,
                price,
                min_quantity,
                max_quantity,
                leverage,
                liquidation_price,
                creation_timestamp,
                term,
                origin,
                oracle_event_id,
            };

            Cfd {
                order,
                quantity_usd: quantity,
                state: latest_state,
            }
        })
        .collect();

    Ok(cfds)
}

/// Loads all CFDs with the latest state as the CFD state
pub async fn load_cfds_by_oracle_event_id(
    oracle_event_id: BitMexPriceEventId,
    conn: &mut PoolConnection<Sqlite>,
) -> anyhow::Result<Vec<Cfd>> {
    let oracle_event_id = oracle_event_id.to_string();

    let rows = sqlx::query!(
        r#"
        select
            orders.uuid as order_id,
            orders.initial_price as price,
            orders.min_quantity as min_quantity,
            orders.max_quantity as max_quantity,
            orders.leverage as leverage,
            orders.trading_pair as trading_pair,
            orders.position as position,
            orders.origin as origin,
            orders.liquidation_price as liquidation_price,
            orders.creation_timestamp as creation_timestamp,
            orders.term as term,
            orders.oracle_event_id,
            cfds.quantity_usd as quantity_usd,
            cfd_states.state as state
        from cfds as cfds
        inner join orders as orders on cfds.order_id = orders.id
        inner join cfd_states as cfd_states on cfd_states.cfd_id = cfds.id
        where cfd_states.state in (
            select
              state
              from cfd_states
            where cfd_id = cfds.id
            order by id desc
            limit 1
        )
        and orders.oracle_event_id = ?
        "#,
        oracle_event_id
    )
    .fetch_all(conn)
    .await?;

    let cfds = rows
        .iter()
        .map(|row| {
            let order_id = serde_json::from_str(row.order_id.as_str()).unwrap();
            let trading_pair = serde_json::from_str(row.trading_pair.as_str()).unwrap();
            let position: Position = serde_json::from_str(row.position.as_str()).unwrap();
            let price = serde_json::from_str(row.price.as_str()).unwrap();
            let min_quantity = serde_json::from_str(row.min_quantity.as_str()).unwrap();
            let max_quantity = serde_json::from_str(row.max_quantity.as_str()).unwrap();
            let leverage = Leverage(row.leverage.try_into().unwrap());
            let liquidation_price = serde_json::from_str(row.liquidation_price.as_str()).unwrap();
            let creation_timestamp = serde_json::from_str(row.creation_timestamp.as_str()).unwrap();
            let term = serde_json::from_str(row.term.as_str()).unwrap();
            let origin: Origin = serde_json::from_str(row.origin.as_str()).unwrap();
            let oracle_event_id = row.oracle_event_id.parse().unwrap();

            let quantity = serde_json::from_str(row.quantity_usd.as_str()).unwrap();
            let latest_state = serde_json::from_str(row.state.as_str()).unwrap();

            let order = Order {
                id: order_id,
                trading_pair,
                position,
                price,
                min_quantity,
                max_quantity,
                leverage,
                liquidation_price,
                creation_timestamp,
                term,
                origin,
                oracle_event_id,
            };

            Cfd {
                order,
                quantity_usd: quantity,
                state: latest_state,
            }
        })
        .collect();

    Ok(cfds)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::SystemTime;

    use rust_decimal_macros::dec;
    use sqlx::SqlitePool;
    use tempfile::tempdir;
    use time::macros::datetime;
    use time::OffsetDateTime;

    use crate::db::insert_order;
    use crate::model::cfd::{Cfd, CfdState, CfdStateCommon, Order};
    use crate::model::Usd;

    use super::*;

    #[tokio::test]
    async fn test_insert_and_load_order() {
        let mut conn = setup_test_db().await;

        let order = Order::default();
        insert_order(&order, &mut conn).await.unwrap();

        let order_loaded = load_order_by_id(order.id, &mut conn).await.unwrap();

        assert_eq!(order, order_loaded);
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd() {
        let mut conn = setup_test_db().await;

        let cfd = Cfd::default();

        insert_order(&cfd.order, &mut conn).await.unwrap();
        insert_cfd(cfd.clone(), &mut conn).await.unwrap();

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        let cfd_from_db = cfds_from_db.first().unwrap().clone();
        assert_eq!(cfd, cfd_from_db)
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_by_order_id() {
        let mut conn = setup_test_db().await;

        let cfd = Cfd::default();
        let order_id = cfd.order.id;

        insert_order(&cfd.order, &mut conn).await.unwrap();
        insert_cfd(cfd.clone(), &mut conn).await.unwrap();

        let cfd_from_db = load_cfd_by_order_id(order_id, &mut conn).await.unwrap();
        assert_eq!(cfd, cfd_from_db)
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_by_order_id_multiple() {
        let mut conn = setup_test_db().await;

        let cfd = Cfd::default();
        let order_id = cfd.order.id;

        insert_order(&cfd.order, &mut conn).await.unwrap();
        insert_cfd(cfd.clone(), &mut conn).await.unwrap();

        let cfd_from_db = load_cfd_by_order_id(order_id, &mut conn).await.unwrap();
        assert_eq!(cfd, cfd_from_db);

        let cfd = Cfd::default();
        let order_id = cfd.order.id;

        insert_order(&cfd.order, &mut conn).await.unwrap();
        insert_cfd(cfd.clone(), &mut conn).await.unwrap();

        let cfd_from_db = load_cfd_by_order_id(order_id, &mut conn).await.unwrap();
        assert_eq!(cfd, cfd_from_db);
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_by_oracle_event_id() {
        let mut conn = setup_test_db().await;

        let oracle_event_id_1 =
            BitMexPriceEventId::with_20_digits(datetime!(2021-10-13 10:00:00).assume_utc());
        let oracle_event_id_2 =
            BitMexPriceEventId::with_20_digits(datetime!(2021-10-25 18:00:00).assume_utc());

        let cfd_1 =
            Cfd::default().with_order(Order::default().with_oracle_event_id(oracle_event_id_1));

        insert_order(&cfd_1.order, &mut conn).await.unwrap();
        insert_cfd(cfd_1.clone(), &mut conn).await.unwrap();

        let cfd_from_db = load_cfds_by_oracle_event_id(oracle_event_id_1, &mut conn)
            .await
            .unwrap();
        assert_eq!(vec![cfd_1.clone()], cfd_from_db);

        let cfd_2 =
            Cfd::default().with_order(Order::default().with_oracle_event_id(oracle_event_id_1));

        insert_order(&cfd_2.order, &mut conn).await.unwrap();
        insert_cfd(cfd_2.clone(), &mut conn).await.unwrap();

        let cfd_from_db = load_cfds_by_oracle_event_id(oracle_event_id_1, &mut conn)
            .await
            .unwrap();
        assert_eq!(vec![cfd_1, cfd_2], cfd_from_db);

        let cfd_3 =
            Cfd::default().with_order(Order::default().with_oracle_event_id(oracle_event_id_2));

        insert_order(&cfd_3.order, &mut conn).await.unwrap();
        insert_cfd(cfd_3.clone(), &mut conn).await.unwrap();

        let cfd_from_db = load_cfds_by_oracle_event_id(oracle_event_id_2, &mut conn)
            .await
            .unwrap();
        assert_eq!(vec![cfd_3], cfd_from_db);
    }

    #[tokio::test]
    async fn test_insert_new_cfd_state_and_load_with_multiple_cfd() {
        let mut conn = setup_test_db().await;

        let mut cfd_1 = Cfd::default();

        insert_order(&cfd_1.order, &mut conn).await.unwrap();
        insert_cfd(cfd_1.clone(), &mut conn).await.unwrap();

        cfd_1.state = CfdState::Accepted {
            common: CfdStateCommon {
                transition_timestamp: SystemTime::now(),
            },
        };
        insert_new_cfd_state_by_order_id(cfd_1.order.id, &cfd_1.state, &mut conn)
            .await
            .unwrap();

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        assert_eq!(cfds_from_db.len(), 1);

        let cfd_from_db = cfds_from_db.first().unwrap().clone();
        assert_eq!(cfd_1, cfd_from_db);

        let mut cfd_2 = Cfd::default();

        insert_order(&cfd_2.order, &mut conn).await.unwrap();
        insert_cfd(cfd_2.clone(), &mut conn).await.unwrap();

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        assert_eq!(cfds_from_db.len(), 2);

        assert!(cfds_from_db.contains(&cfd_1));
        assert!(cfds_from_db.contains(&cfd_2));

        cfd_2.state = CfdState::Rejected {
            common: CfdStateCommon {
                transition_timestamp: SystemTime::now(),
            },
        };
        insert_new_cfd_state_by_order_id(cfd_2.order.id, &cfd_2.state, &mut conn)
            .await
            .unwrap();

        let cfds_from_db = load_all_cfds(&mut conn).await.unwrap();
        assert_eq!(cfds_from_db.len(), 2);

        assert!(cfds_from_db.contains(&cfd_1));
        assert!(cfds_from_db.contains(&cfd_2));
    }

    async fn setup_test_db() -> PoolConnection<Sqlite> {
        let temp_db = tempdir().unwrap().into_path().join("tempdb");

        // file has to exist in order to connect with sqlite
        File::create(temp_db.clone()).unwrap();

        let pool = SqlitePool::connect(format!("sqlite:{}", temp_db.display()).as_str())
            .await
            .unwrap();

        run_migrations(&pool).await.unwrap();

        pool.acquire().await.unwrap()
    }

    impl Default for Cfd {
        fn default() -> Self {
            Cfd::new(
                Order::default(),
                Usd(dec!(1000)),
                CfdState::OutgoingOrderRequest {
                    common: CfdStateCommon {
                        transition_timestamp: SystemTime::now(),
                    },
                },
            )
        }
    }

    impl Cfd {
        pub fn with_order(mut self, order: Order) -> Self {
            self.order = order;
            self
        }
    }

    impl Default for Order {
        fn default() -> Self {
            Order::new(
                Usd(dec!(1000)),
                Usd(dec!(100)),
                Usd(dec!(1000)),
                Origin::Theirs,
                BitMexPriceEventId::with_20_digits(OffsetDateTime::now_utc()),
            )
            .unwrap()
        }
    }

    impl Order {
        pub fn with_oracle_event_id(mut self, oracle_event_id: BitMexPriceEventId) -> Self {
            self.oracle_event_id = oracle_event_id;
            self
        }
    }
}
