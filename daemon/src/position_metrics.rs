use crate::db;
use async_trait::async_trait;
use futures::StreamExt;
use model::CfdEvent;
use model::EventKind;
use model::Position;
use model::Usd;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::time::Duration;
use tokio_tasks::Tasks;
use xtra_productivity::xtra_productivity;
use xtras::SendInterval;

// TODO: ideally this would be more often
pub const UPDATE_METRIC_INTERVAL_SECONDS: Duration = Duration::from_secs(60);

pub struct Actor {
    tasks: Tasks,
    db: db::Connection,
}

impl Actor {
    pub fn new(db: db::Connection) -> Self {
        Self {
            db,
            tasks: Tasks::default(),
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let this = ctx.address().expect("we are alive");
        self.tasks
            .add(this.send_interval(UPDATE_METRIC_INTERVAL_SECONDS, || UpdateMetrics));
    }

    async fn stopped(self) -> Self::Stop {}
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, _: UpdateMetrics) {
        tracing::debug!("Collecting metrics");
        let mut stream = self.db.load_all_cfds::<Cfd>(());

        let mut cfds = Vec::new();
        while let Some(cfd) = stream.next().await {
            let cfd = match cfd {
                Ok(cfd) => cfd,
                Err(e) => {
                    tracing::error!("Failed to rehydrate CFD: {e:#}");
                    continue;
                }
            };
            cfds.push(cfd);
        }

        metrics::update_position_metrics(cfds.as_slice());
    }
}

#[derive(Debug)]
struct UpdateMetrics;

/// Read-model of the CFD for the position metrics actor.
#[derive(Clone, Copy)]
pub struct Cfd {
    position: Position,
    quantity_usd: Usd,

    is_open: bool,
    is_closed: bool,
    is_failed: bool,
    is_refunded: bool,

    version: u32,
}

impl db::CfdAggregate for Cfd {
    type CtorArgs = ();

    fn new(_: Self::CtorArgs, cfd: db::Cfd) -> Self {
        Self {
            position: cfd.position,
            quantity_usd: cfd.quantity_usd,
            is_open: false,
            is_closed: false,
            is_failed: false,
            is_refunded: false,
            version: 0,
        }
    }

    fn apply(self, event: CfdEvent) -> Self {
        self.apply(event)
    }

    fn version(&self) -> u32 {
        self.version
    }
}

impl Cfd {
    fn apply(mut self, event: CfdEvent) -> Self {
        self.version += 1;
        use EventKind::*;
        match event.event {
            ContractSetupStarted => Self {
                is_open: false,
                is_closed: false,
                is_failed: false,
                is_refunded: false,
                ..self
            },
            ContractSetupCompleted { .. } | LockConfirmed | LockConfirmedAfterFinality => Self {
                is_open: true,
                ..self
            },
            ContractSetupFailed => Self {
                is_failed: true,
                ..self
            },
            OfferRejected => Self {
                is_failed: true,
                ..self
            },
            RolloverStarted
            | RolloverAccepted
            | RolloverRejected
            | RolloverCompleted { .. }
            | RolloverFailed => Self {
                // should still be open
                ..self
            },
            CollaborativeSettlementStarted { .. }
            | CollaborativeSettlementProposalAccepted
            | CollaborativeSettlementRejected
            | CollaborativeSettlementFailed => Self {
                // should still be open
                ..self
            },
            CollaborativeSettlementCompleted { .. } => Self {
                is_open: false,
                is_closed: true,
                ..self
            },
            ManualCommit { .. } | CommitConfirmed => Self {
                // we don't know yet if the position will be closed immediately (e.g. through
                // punishing) or a bit later after the oracle has attested to the price
                ..self
            },
            CetConfirmed => Self {
                is_open: false,
                is_closed: true,
                ..self
            },
            RefundConfirmed => Self {
                is_open: false,
                is_refunded: true,
                ..self
            },
            RevokeConfirmed => Self {
                // the other party was punished, we are done here!
                is_open: false,
                is_closed: true,
                ..self
            },
            CollaborativeSettlementConfirmed => Self {
                is_open: false,
                is_closed: true,
                ..self
            },
            CetTimelockExpiredPriorOracleAttestation
            | CetTimelockExpiredPostOracleAttestation { .. } => Self {
                is_open: false,
                is_closed: true,
                ..self
            },
            RefundTimelockExpired { .. } => Self {
                // a rollover with an expired timelock should be rejected for settlement and
                // rollover, hence, this is closed
                is_open: false,
                is_closed: true,
                ..self
            },
            OracleAttestedPriorCetTimelock { .. } | OracleAttestedPostCetTimelock { .. } => Self {
                // we know the closing price already and can assume that the cfd will be closed
                // accordingly
                is_open: false,
                is_closed: true,
                ..self
            },
        }
    }
}

impl db::ClosedCfdAggregate for Cfd {
    fn new_closed(_: Self::CtorArgs, closed_cfd: db::ClosedCfd) -> Self {
        let db::ClosedCfd {
            position,
            n_contracts,
            ..
        } = closed_cfd;

        let quantity_usd =
            Usd::new(Decimal::from_u64(u64::from(n_contracts)).expect("u64 to fit into Decimal"));

        Self {
            position,
            quantity_usd,

            is_open: false,
            is_closed: true,
            is_failed: false,
            is_refunded: false,
            version: 0,
        }
    }
}

mod metrics {
    use crate::position_metrics::Cfd;
    use model::Position;
    use model::Usd;
    use rust_decimal::prelude::ToPrimitive;
    use std::collections::HashMap;
    use std::iter::Filter;
    use std::slice::Iter;

    const POSITION_LABEL: &str = "position";
    const POSITION_LONG_LABEL: &str = "long";
    const POSITION_SHORT_LABEL: &str = "short";

    const STATUS_LABEL: &str = "status";
    const STATUS_OPEN_LABEL: &str = "open";
    const STATUS_CLOSED_LABEL: &str = "closed";
    const STATUS_FAILED_LABEL: &str = "failed";
    const STATUS_REFUNDED_LABEL: &str = "refunded";

    static POSITION_QUANTITY_GAUGE: conquer_once::Lazy<prometheus::GaugeVec> =
        conquer_once::Lazy::new(|| {
            prometheus::register_gauge_vec!(
                "positions_quantity",
                "Total quantity of positions on ItchySats.",
                &[POSITION_LABEL, STATUS_LABEL]
            )
            .unwrap()
        });

    static POSITION_AMOUNT_GAUGE: conquer_once::Lazy<prometheus::IntGaugeVec> =
        conquer_once::Lazy::new(|| {
            prometheus::register_int_gauge_vec!(
                "positions_total",
                "Total number of positions on ItchySats.",
                &[POSITION_LABEL, STATUS_LABEL]
            )
            .unwrap()
        });

    pub fn update_position_metrics(cfds: &[Cfd]) {
        set_position_metrics(cfds.iter().filter(|cfd| cfd.is_open), STATUS_OPEN_LABEL);
        set_position_metrics(cfds.iter().filter(|cfd| cfd.is_closed), STATUS_CLOSED_LABEL);
        set_position_metrics(cfds.iter().filter(|cfd| cfd.is_failed), STATUS_FAILED_LABEL);
        set_position_metrics(
            cfds.iter().filter(|cfd| cfd.is_refunded),
            STATUS_REFUNDED_LABEL,
        );
    }

    fn set_position_metrics(cfds: Filter<Iter<Cfd>, fn(&&Cfd) -> bool>, status: &str) {
        let (long, short): (Vec<_>, Vec<_>) = cfds.partition(|cfd| cfd.position == Position::Long);

        POSITION_QUANTITY_GAUGE
            .with(&HashMap::from([
                (POSITION_LABEL, POSITION_LONG_LABEL),
                (STATUS_LABEL, status),
            ]))
            .set(
                sum_amounts(&long)
                    .into_decimal()
                    .to_f64()
                    .unwrap_or_default(),
            );
        POSITION_AMOUNT_GAUGE
            .with(&HashMap::from([
                (POSITION_LABEL, POSITION_LONG_LABEL),
                (STATUS_LABEL, status),
            ]))
            .set(long.len() as i64);

        POSITION_QUANTITY_GAUGE
            .with(&HashMap::from([
                (POSITION_LABEL, POSITION_SHORT_LABEL),
                (STATUS_LABEL, status),
            ]))
            .set(
                sum_amounts(&short)
                    .into_decimal()
                    .to_f64()
                    .unwrap_or_default(),
            );
        POSITION_AMOUNT_GAUGE
            .with(&HashMap::from([
                (POSITION_LABEL, POSITION_SHORT_LABEL),
                (STATUS_LABEL, status),
            ]))
            .set(short.len() as i64);
    }

    fn sum_amounts(cfds: &[&Cfd]) -> Usd {
        cfds.iter()
            .fold(Usd::ZERO, |sum, cfd| cfd.quantity_usd + sum)
    }
}
