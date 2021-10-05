use crate::db::{insert_new_cfd_state_by_order_id, load_all_cfds};
use crate::model::cfd::{Cfd, CfdState, CfdStateCommon};
use crate::wallet::Wallet;
use anyhow::Result;
use sqlx::pool::PoolConnection;
use sqlx::Sqlite;
use std::time::SystemTime;

pub async fn transition_non_continue_cfds_to_setup_failed(
    conn: &mut PoolConnection<Sqlite>,
) -> Result<()> {
    let cfds = load_all_cfds(conn).await?;

    for cfd in cfds.iter().filter(|cfd| Cfd::is_cleanup(cfd)) {
        insert_new_cfd_state_by_order_id(
            cfd.order.id,
            CfdState::SetupFailed {
                common: CfdStateCommon {
                    transition_timestamp: SystemTime::now(),
                },
                info: format!("Was in state {} which cannot be continued.", cfd.state),
            },
            conn,
        )
        .await?;
    }

    Ok(())
}

pub async fn rebroadcast_transactions(
    conn: &mut PoolConnection<Sqlite>,
    wallet: &Wallet,
) -> Result<()> {
    let cfds = load_all_cfds(conn).await?;

    for dlc in cfds.iter().filter_map(|cfd| Cfd::pending_open_dlc(cfd)) {
        let txid = wallet.try_broadcast_transaction(dlc.lock.0.clone()).await?;

        tracing::info!("Lock transaction published with txid {}", txid);
    }

    for cfd in cfds.iter().filter(|cfd| Cfd::is_must_refund(cfd)) {
        let signed_refund_tx = cfd.refund_tx()?;
        let txid = wallet.try_broadcast_transaction(signed_refund_tx).await?;

        tracing::info!("Refund transaction published on chain: {}", txid);
    }

    for cfd in cfds.iter().filter(|cfd| Cfd::is_pending_commit(cfd)) {
        let signed_commit_tx = cfd.commit_tx()?;
        let txid = wallet.try_broadcast_transaction(signed_commit_tx).await?;

        tracing::info!("Commit transaction published on chain: {}", txid);
    }

    Ok(())
}