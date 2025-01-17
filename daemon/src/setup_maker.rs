use crate::command;
use crate::db;
use crate::maker_inc_connections;
use crate::process_manager;
use crate::setup_contract;
use crate::wallet;
use crate::wire;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::future;
use futures::SinkExt;
use maia::secp256k1_zkp::schnorrsig;
use model::olivia::Announcement;
use model::Dlc;
use model::Identity;
use model::Order;
use model::Role;
use model::Usd;
use tokio_tasks::Tasks;
use xtra::prelude::MessageChannel;
use xtra::KeepRunning;
use xtra_productivity::xtra_productivity;
use xtras::address_map::IPromiseIamReturningStopAllFromStopping;
use xtras::LogFailure;

pub struct Actor {
    order: Order,
    quantity: Usd,
    n_payouts: usize,
    oracle_pk: schnorrsig::PublicKey,
    announcement: Announcement,
    build_party_params: Box<dyn MessageChannel<wallet::BuildPartyParams>>,
    sign: Box<dyn MessageChannel<wallet::Sign>>,
    taker: Box<dyn MessageChannel<maker_inc_connections::TakerMessage>>,
    confirm_order: Box<dyn MessageChannel<maker_inc_connections::ConfirmOrder>>,
    taker_id: Identity,
    setup_msg_sender: Option<UnboundedSender<wire::SetupMsg>>,
    tasks: Tasks,
    executor: command::Executor,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: db::Connection,
        process_manager: xtra::Address<process_manager::Actor>,
        (order, quantity, n_payouts): (Order, Usd, usize),
        (oracle_pk, announcement): (schnorrsig::PublicKey, Announcement),
        build_party_params: &(impl MessageChannel<wallet::BuildPartyParams> + 'static),
        sign: &(impl MessageChannel<wallet::Sign> + 'static),
        (taker, confirm_order, taker_id): (
            &(impl MessageChannel<maker_inc_connections::TakerMessage> + 'static),
            &(impl MessageChannel<maker_inc_connections::ConfirmOrder> + 'static),
            Identity,
        ),
    ) -> Self {
        Self {
            executor: command::Executor::new(db, process_manager),
            order,
            quantity,
            n_payouts,
            oracle_pk,
            announcement,
            build_party_params: build_party_params.clone_channel(),
            sign: sign.clone_channel(),
            taker: taker.clone_channel(),
            confirm_order: confirm_order.clone_channel(),
            taker_id,
            setup_msg_sender: None,
            tasks: Tasks::default(),
        }
    }

    async fn contract_setup(&mut self, this: xtra::Address<Self>) -> Result<()> {
        let order_id = self.order.id;

        let (sender, receiver) = mpsc::unbounded();
        // store the writing end to forward messages from the taker to
        // the spawned contract setup task
        self.setup_msg_sender = Some(sender);

        let (setup_params, position) = self
            .executor
            .execute(order_id, |cfd| cfd.start_contract_setup())
            .await?;

        let taker_id = setup_params.counterparty_identity();

        let contract_future = setup_contract::new(
            self.taker.sink().with(move |msg| {
                future::ok(maker_inc_connections::TakerMessage {
                    taker_id,
                    msg: wire::MakerToTaker::Protocol { order_id, msg },
                })
            }),
            receiver,
            (self.oracle_pk, self.announcement.clone()),
            setup_params,
            self.build_party_params.clone_channel(),
            self.sign.clone_channel(),
            Role::Maker,
            position,
            self.n_payouts,
        );

        self.tasks.add(async move {
            let _: Result<(), xtra::Disconnected> = match contract_future.await {
                Ok(dlc) => this.send(SetupSucceeded { dlc }).await,
                Err(error) => this.send(SetupFailed { error }).await,
            };
        });

        Ok(())
    }

    async fn emit_complete(&mut self, dlc: Dlc, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order.id, |cfd| cfd.complete_contract_setup(dlc))
            .await
        {
            tracing::error!("Failed to execute `complete_contract_setup` command: {e:#}");
        }

        ctx.stop();
    }

    async fn emit_reject(&mut self, reason: anyhow::Error, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order.id, |cfd| cfd.reject_contract_setup(reason))
            .await
        {
            tracing::error!("Failed to execute `reject_contract_setup` command: {e:#}");
        }

        ctx.stop();
    }

    async fn emit_fail(&mut self, error: anyhow::Error, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order.id, |cfd| Ok(cfd.fail_contract_setup(error)))
            .await
        {
            tracing::error!("Failed to execute `fail_contract_setup` command: {e:#}");
        }

        ctx.stop();
    }

    async fn forward_protocol_msg(&self, msg: wire::SetupMsg) -> Result<()> {
        let mut sender = self
            .setup_msg_sender
            .clone()
            .context("Cannot forward message to contract setup task")?;
        sender.send(msg).await?;

        Ok(())
    }
}

#[xtra_productivity]
impl Actor {
    fn handle(&mut self, _msg: Accepted, ctx: &mut xtra::Context<Self>) {
        let order_id = self.order.id;

        if self.setup_msg_sender.is_some() {
            tracing::warn!(%order_id, "Contract setup already active");
            return;
        }

        tracing::info!(%order_id, "Maker accepts an order");

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");

        let fut = async {
            self.confirm_order
                .send(maker_inc_connections::ConfirmOrder {
                    taker_id: self.taker_id,
                    order_id,
                    address: this.clone(),
                })
                .await
                .context("Failed to deliver order confirmation")??;

            self.contract_setup(this)
                .await
                .context("Failed to start contract setup")?;

            Ok(())
        };

        if let Err(error) = fut.await {
            tracing::warn!(%order_id, "Stopping setup_maker actor: {error}");

            self.emit_fail(error, ctx).await;

            return;
        }
    }

    fn handle(&mut self, _msg: Rejected, ctx: &mut xtra::Context<Self>) {
        let _ = self
            .taker
            .send(maker_inc_connections::TakerMessage {
                taker_id: self.taker_id,
                msg: wire::MakerToTaker::RejectOrder(self.order.id),
            })
            .log_failure("Failed to reject order to taker")
            .await;

        self.emit_reject(anyhow::format_err!("unknown"), ctx).await
    }

    fn handle(&mut self, msg: SetupSucceeded, ctx: &mut xtra::Context<Self>) {
        self.emit_complete(msg.dlc, ctx).await
    }

    fn handle(&mut self, msg: SetupFailed, ctx: &mut xtra::Context<Self>) {
        self.emit_fail(msg.error, ctx).await
    }
}

#[xtra_productivity(message_impl = false)]
impl Actor {
    fn handle(&mut self, msg: wire::SetupMsg) {
        if let Err(e) = self.forward_protocol_msg(msg).await {
            tracing::error!("Failed to forward protocol message: {e:#}")
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let quantity = self.quantity;
        if quantity < self.order.min_quantity || quantity > self.order.max_quantity {
            let min = self.order.min_quantity;
            let max = self.order.max_quantity;

            let reason =
                format!("Order rejected: quantity {quantity} not in range [{min}, {max}]",);
            tracing::info!("{reason}");

            let _ = self
                .taker
                .send(maker_inc_connections::TakerMessage {
                    taker_id: self.taker_id,
                    msg: wire::MakerToTaker::RejectOrder(self.order.id),
                })
                .await;

            self.emit_reject(anyhow::format_err!(reason), ctx).await;
        }
    }

    async fn stopping(&mut self, _: &mut xtra::Context<Self>) -> KeepRunning {
        KeepRunning::StopAll
    }

    async fn stopped(self) -> Self::Stop {}
}

impl IPromiseIamReturningStopAllFromStopping for Actor {}

/// Message sent from the `maker_cfd::Actor` to the
/// `setup_maker::Actor` to inform that the maker user has accepted
/// the taker order request from the taker.
#[derive(Clone, Copy)]
pub struct Accepted;

/// Message sent from the `maker_cfd::Actor` to the
/// `setup_maker::Actor` to inform that the maker user has rejected
/// the taker order request from the taker.
#[derive(Clone, Copy)]
pub struct Rejected;

/// Message sent from the spawned task to `setup_maker::Actor` to
/// notify that the contract setup has finished successfully.
struct SetupSucceeded {
    dlc: Dlc,
}

/// Message sent from the spawned task to `setup_maker::Actor` to
/// notify that the contract setup has failed.
struct SetupFailed {
    error: anyhow::Error,
}
