use crate::command;
use crate::db;
use crate::maker_inc_connections;
use crate::oracle;
use crate::process_manager;
use crate::schnorrsig;
use crate::setup_contract;
use crate::wire;
use anyhow::Context as _;
use anyhow::Result;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::future;
use futures::SinkExt;
use model::Dlc;
use model::FundingFee;
use model::FundingRate;
use model::Identity;
use model::OrderId;
use model::Position;
use model::Role;
use model::RolloverVersion;
use model::TxFeeRate;
use tokio_tasks::Tasks;
use xtra::prelude::MessageChannel;
use xtra::KeepRunning;
use xtra_productivity::xtra_productivity;
use xtras::address_map::IPromiseIamReturningStopAllFromStopping;

/// Upon accepting Rollover maker sends the current estimated transaction fee and
/// funding rate
#[derive(Clone, Copy)]
pub struct AcceptRollover {
    pub tx_fee_rate: TxFeeRate,
    pub long_funding_rate: FundingRate,
    pub short_funding_rate: FundingRate,
}

#[derive(Clone, Copy)]
pub struct RejectRollover;

pub struct ProtocolMsg(pub wire::RolloverMsg);

/// Message sent from the spawned task to `rollover_taker::Actor` to
/// notify that rollover has finished successfully.
struct RolloverSucceeded {
    dlc: Dlc,
    funding_fee: FundingFee,
}

/// Message sent from the spawned task to `rollover_maker::Actor` to
/// notify that rollover has failed.
struct RolloverFailed {
    error: anyhow::Error,
}

pub struct Actor {
    order_id: OrderId,
    send_to_taker_actor: Box<dyn MessageChannel<maker_inc_connections::TakerMessage>>,
    n_payouts: usize,
    taker_id: Identity,
    oracle_pk: schnorrsig::PublicKey,
    sent_from_taker: Option<UnboundedSender<wire::RolloverMsg>>,
    oracle_actor: Box<dyn MessageChannel<oracle::GetAnnouncement>>,
    register: Box<dyn MessageChannel<maker_inc_connections::RegisterRollover>>,
    tasks: Tasks,
    executor: command::Executor,
    version: RolloverVersion,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        order_id: OrderId,
        n_payouts: usize,
        send_to_taker_actor: &(impl MessageChannel<maker_inc_connections::TakerMessage> + 'static),
        taker_id: Identity,
        oracle_pk: schnorrsig::PublicKey,
        oracle_actor: &(impl MessageChannel<oracle::GetAnnouncement> + 'static),
        process_manager: xtra::Address<process_manager::Actor>,
        register: &(impl MessageChannel<maker_inc_connections::RegisterRollover> + 'static),
        db: db::Connection,
        version: RolloverVersion,
    ) -> Self {
        Self {
            order_id,
            n_payouts,
            send_to_taker_actor: send_to_taker_actor.clone_channel(),
            taker_id,
            oracle_pk,
            sent_from_taker: None,
            oracle_actor: oracle_actor.clone_channel(),
            register: register.clone_channel(),
            executor: command::Executor::new(db, process_manager),
            tasks: Tasks::default(),
            version,
        }
    }

    async fn emit_complete(
        &mut self,
        dlc: Dlc,
        funding_fee: FundingFee,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(e) = self
            .executor
            .execute(self.order_id, |cfd| {
                Ok(cfd.complete_rollover(dlc, funding_fee))
            })
            .await
        {
            tracing::warn!(order_id = %self.order_id, "{:#}", e)
        }

        ctx.stop();
    }

    async fn emit_reject(&mut self, reason: anyhow::Error, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order_id, |cfd| Ok(cfd.reject_rollover(reason)))
            .await
        {
            tracing::warn!(order_id = %self.order_id, "{:#}", e)
        }

        ctx.stop();
    }

    async fn emit_fail(&mut self, error: anyhow::Error, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order_id, |cfd| Ok(cfd.fail_rollover(error)))
            .await
        {
            tracing::warn!(order_id = %self.order_id, "{:#}", e)
        }

        ctx.stop();
    }

    async fn accept(&mut self, msg: AcceptRollover, ctx: &mut xtra::Context<Self>) -> Result<()> {
        let order_id = self.order_id;
        let AcceptRollover {
            tx_fee_rate,
            long_funding_rate,
            short_funding_rate,
        } = msg;

        if self.sent_from_taker.is_some() {
            tracing::debug!(%order_id, "Rollover already active");
            return Ok(());
        }

        let (sender, receiver) = mpsc::unbounded();

        self.sent_from_taker = Some(sender);

        let (rollover_params, dlc, position, interval, funding_rate) = self
            .executor
            .execute(self.order_id, |cfd| {
                let funding_rate = match cfd.position() {
                    Position::Long => long_funding_rate,
                    Position::Short => short_funding_rate,
                };

                let (event, params, dlc, position, interval) =
                    cfd.accept_rollover_proposal(tx_fee_rate, funding_rate, self.version)?;

                Ok((event, params, dlc, position, interval, funding_rate))
            })
            .await?;

        let oracle_event_id =
            oracle::next_announcement_after(time::OffsetDateTime::now_utc() + interval);

        let taker_id = self.taker_id;

        self.send_to_taker_actor
            .send(maker_inc_connections::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::ConfirmRollover {
                    order_id,
                    oracle_event_id,
                    tx_fee_rate,
                    funding_rate,
                },
            })
            .await
            .context("Maker connection actor disconnected")?
            .context("Failed to send confirm rollover message")?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await
            .context("Oracle actor disconnected")?
            .context("Failed to get announcement")?;

        let funding_fee = *rollover_params.funding_fee();

        let rollover_fut = setup_contract::roll_over(
            self.send_to_taker_actor.sink().with(move |msg| {
                future::ok(maker_inc_connections::TakerMessage {
                    taker_id,
                    msg: wire::MakerToTaker::RolloverProtocol { order_id, msg },
                })
            }),
            receiver,
            (self.oracle_pk, announcement),
            rollover_params,
            Role::Maker,
            position,
            dlc,
            self.n_payouts,
        );

        let this = ctx.address().expect("self to be alive");

        self.tasks.add(async move {
            let _: Result<(), xtra::Disconnected> =
                match rollover_fut.await.context("Rollover protocol failed") {
                    Ok(dlc) => this.send(RolloverSucceeded { dlc, funding_fee }).await,
                    Err(source) => this.send(RolloverFailed { error: source }).await,
                };
        });

        Ok(())
    }

    async fn reject(&mut self, ctx: &mut xtra::Context<Self>) -> Result<()> {
        tracing::info!(id = %self.order_id, "Rejecting rollover proposal" );

        self.send_to_taker_actor
            .send(maker_inc_connections::TakerMessage {
                taker_id: self.taker_id,
                msg: wire::MakerToTaker::RejectRollover(self.order_id),
            })
            .await
            .context("Maker connection actor disconnected")?
            .context("Failed to send reject rollover message")?;

        self.emit_reject(anyhow::format_err!("unknown"), ctx).await;

        ctx.stop();

        Ok(())
    }

    pub async fn forward_protocol_msg(&mut self, msg: ProtocolMsg) -> Result<()> {
        self.sent_from_taker
            .as_mut()
            .context("Rollover task is not active")? // Sender is set once `Accepted` is sent.
            .send(msg.0)
            .await
            .context("Failed to forward message to rollover task")?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl xtra::Actor for Actor {
    type Stop = ();
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let order_id = self.order_id;
        let taker_id = self.taker_id;

        tracing::info!(
            %order_id,
            %taker_id,
            "Received rollover proposal"
        );

        let this = ctx.address().expect("self to be alive");
        let fut = async {
            // Register ourselves with the actor handling connections with
            // takers, so that it knows where to forward rollover messages
            // which correspond to this instance
            self.register
                .send(maker_inc_connections::RegisterRollover {
                    order_id,
                    address: this,
                })
                .await?;

            self.executor
                .execute(self.order_id, |cfd| cfd.start_rollover())
                .await?;

            anyhow::Ok(())
        };

        if let Err(source) = fut.await {
            self.emit_fail(source, ctx).await;
        }
    }

    async fn stopping(&mut self, _: &mut xtra::Context<Self>) -> KeepRunning {
        KeepRunning::StopAll
    }

    async fn stopped(self) -> Self::Stop {}
}

impl IPromiseIamReturningStopAllFromStopping for Actor {}

#[xtra_productivity]
impl Actor {
    async fn handle_accept_rollover(&mut self, msg: AcceptRollover, ctx: &mut xtra::Context<Self>) {
        if let Err(error) = self.accept(msg, ctx).await {
            self.emit_fail(error, ctx).await;
        };
    }

    async fn handle_reject_rollover(
        &mut self,
        _msg: RejectRollover,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(error) = self.reject(ctx).await {
            self.emit_fail(error, ctx).await;
        };
    }

    async fn handle_protocol_msg(&mut self, msg: ProtocolMsg, ctx: &mut xtra::Context<Self>) {
        if let Err(error) = self.forward_protocol_msg(msg).await {
            self.emit_fail(error, ctx).await;
        };
    }

    async fn handle_rollover_failed(&mut self, msg: RolloverFailed, ctx: &mut xtra::Context<Self>) {
        self.emit_fail(msg.error, ctx).await
    }

    async fn handle_rollover_succeeded(
        &mut self,
        msg: RolloverSucceeded,
        ctx: &mut xtra::Context<Self>,
    ) {
        self.emit_complete(msg.dlc, msg.funding_fee, ctx).await
    }
}
