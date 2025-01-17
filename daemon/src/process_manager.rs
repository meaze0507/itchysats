use crate::db;
use crate::monitor::MonitorCetFinality;
use crate::monitor::MonitorCollaborativeSettlement;
use crate::monitor::MonitorParams;
use crate::monitor::StartMonitoring;
use crate::monitor::TransactionKind;
use crate::monitor::TryBroadcastTransaction;
use crate::oracle;
use crate::projection;
use anyhow::Result;
use async_trait::async_trait;
use model::CfdEvent;
use model::EventKind;
use model::Role;
use xtra::prelude::MessageChannel;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

pub struct Actor {
    db: db::Connection,
    role: Role,
    cfds_changed: Box<dyn MessageChannel<projection::CfdChanged>>,
    try_broadcast_transaction: Box<dyn MessageChannel<TryBroadcastTransaction>>,
    start_monitoring: Box<dyn MessageChannel<StartMonitoring>>,
    monitor_cet_finality: Box<dyn MessageChannel<MonitorCetFinality>>,
    monitor_collaborative_settlement: Box<dyn MessageChannel<MonitorCollaborativeSettlement>>,
    monitor_attestation: Box<dyn MessageChannel<oracle::MonitorAttestation>>,
}

pub struct Event(CfdEvent);

impl Event {
    pub fn new(event: CfdEvent) -> Self {
        Self(event)
    }
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: db::Connection,
        role: Role,
        cfds_changed: &(impl MessageChannel<projection::CfdChanged> + 'static),
        try_broadcast_transaction: &(impl MessageChannel<TryBroadcastTransaction> + 'static),
        start_monitoring: &(impl MessageChannel<StartMonitoring> + 'static),
        monitor_cet: &(impl MessageChannel<MonitorCetFinality> + 'static),
        monitor_collaborative_settlement: &(impl MessageChannel<MonitorCollaborativeSettlement>
              + 'static),
        monitor_attestation: &(impl MessageChannel<oracle::MonitorAttestation> + 'static),
    ) -> Self {
        Self {
            db,
            role,
            cfds_changed: cfds_changed.clone_channel(),
            try_broadcast_transaction: try_broadcast_transaction.clone_channel(),
            start_monitoring: start_monitoring.clone_channel(),
            monitor_cet_finality: monitor_cet.clone_channel(),
            monitor_collaborative_settlement: monitor_collaborative_settlement.clone_channel(),
            monitor_attestation: monitor_attestation.clone_channel(),
        }
    }
}

#[xtra_productivity]
impl Actor {
    fn handle(&mut self, msg: Event) -> Result<()> {
        let event = msg.0;

        // 1. Safe in DB
        self.db.append_event(event.clone()).await?;

        // 2. Post process event
        use EventKind::*;
        match event.event {
            ContractSetupCompleted { dlc, .. } => {
                let lock_tx = dlc.lock.0.clone();
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: lock_tx,
                        kind: TransactionKind::Lock,
                    })
                    .await?;

                self.start_monitoring
                    .send_async_safe(StartMonitoring {
                        id: event.id,
                        params: MonitorParams::new(dlc.clone()),
                    })
                    .await?;

                self.monitor_attestation
                    .send_async_safe(oracle::MonitorAttestation {
                        event_id: dlc.settlement_event_id,
                    })
                    .await?;
            }
            CollaborativeSettlementCompleted {
                spend_tx, script, ..
            } => {
                let txid = spend_tx.txid();

                match self.role {
                    Role::Maker => {
                        self.try_broadcast_transaction
                            .send_async_safe(TryBroadcastTransaction {
                                tx: spend_tx,
                                kind: TransactionKind::CollaborativeClose,
                            })
                            .await?;
                    }
                    Role::Taker => {
                        // TODO: Publish the tx once the collaborative settlement is symmetric,
                        // allowing the taker to publish as well.
                    }
                };

                self.monitor_collaborative_settlement
                    .send_async_safe(MonitorCollaborativeSettlement {
                        order_id: event.id,
                        tx: (txid, script),
                    })
                    .await?;
            }
            CetTimelockExpiredPostOracleAttestation { cet } => {
                let _ = self
                    .monitor_cet_finality
                    .send_async_safe(MonitorCetFinality {
                        order_id: event.id,
                        cet: cet.clone(),
                    })
                    .await?;
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: cet,
                        kind: TransactionKind::Cet,
                    })
                    .await?;
            }
            OracleAttestedPostCetTimelock { cet, .. } => {
                let _ = self
                    .monitor_cet_finality
                    .send_async_safe(MonitorCetFinality {
                        order_id: event.id,
                        cet: cet.clone(),
                    })
                    .await?;
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: cet,
                        kind: TransactionKind::Cet,
                    })
                    .await?;
            }
            OracleAttestedPriorCetTimelock {
                commit_tx: Some(commit_tx),
                ..
            } => {
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: commit_tx,
                        kind: TransactionKind::Commit,
                    })
                    .await?;
            }
            ManualCommit { tx } => {
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx,
                        kind: TransactionKind::Commit,
                    })
                    .await?;
            }
            OracleAttestedPriorCetTimelock {
                commit_tx: None,
                timelocked_cet: cet,
                ..
            } => {
                let _ = self
                    .monitor_cet_finality
                    .send_async_safe(MonitorCetFinality {
                        order_id: event.id,
                        cet,
                    })
                    .await?;
            }
            RolloverCompleted { dlc, .. } => {
                self.start_monitoring
                    .send_async_safe(StartMonitoring {
                        id: event.id,
                        params: MonitorParams::new(dlc.clone()),
                    })
                    .await?;

                self.monitor_attestation
                    .send_async_safe(oracle::MonitorAttestation {
                        event_id: dlc.settlement_event_id,
                    })
                    .await?;
            }
            RefundTimelockExpired { refund_tx: tx } => {
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx,
                        kind: TransactionKind::Refund,
                    })
                    .await?;
            }
            RefundConfirmed
            | CollaborativeSettlementStarted { .. }
            | ContractSetupStarted
            | ContractSetupFailed
            | OfferRejected
            | RolloverStarted
            | RolloverAccepted
            | RolloverRejected
            | RolloverFailed
            | CollaborativeSettlementProposalAccepted
            | LockConfirmed
            | LockConfirmedAfterFinality
            | CommitConfirmed
            | CetConfirmed
            | RevokeConfirmed
            | CollaborativeSettlementConfirmed
            | CollaborativeSettlementRejected
            | CollaborativeSettlementFailed
            | CetTimelockExpiredPriorOracleAttestation => {}
        }

        // 3. Update UI
        self.cfds_changed
            .send_async_safe(projection::CfdChanged(event.id))
            .await?;

        Ok(())
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}
