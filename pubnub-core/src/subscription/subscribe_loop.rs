use super::registry::Registry as GenericRegistry;
use crate::data::request;
use crate::data::timetoken::Timetoken;
use crate::transport::Transport;
use futures_channel::{mpsc, oneshot};
use futures_util::future::{select, Either, FutureExt};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use log::{debug, error};

pub(crate) use super::channel::{Rx as ChannelRx, Tx as ChannelTx};
pub(crate) use super::registry::ID as SubscriptionID;

pub(crate) type Registry = GenericRegistry<ChannelTx>;

pub(crate) type ReadyTx = oneshot::Sender<()>;

pub(crate) type ExitTx = mpsc::Sender<()>;

pub(crate) type ControlTx = mpsc::Sender<ControlCommand>;
pub(crate) type ControlRx = mpsc::Receiver<ControlCommand>;

pub(crate) type SubscriptionIdTx = oneshot::Sender<SubscriptionID>;

/// Commands we pass via the control pipe.
#[derive(Debug)]
pub(crate) enum ControlCommand {
    /// A stream for a channel or channel group is being dropped.
    ///
    /// Only sent from `Subscription` to `SubscribeLoop`.
    Drop(SubscriptionID, ListenerType),

    /// A stream for a channel or channel group is being created.
    ///
    /// Only sent from `PubNub` to `SubscribeLoop`.
    Add(ListenerType, ChannelTx, SubscriptionIdTx),
}

/// # Type of listener (a channel or a channel group)
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListenerType {
    Channel(String),      // Channel name
    ChannelGroup(String), // Channel Group name
}

#[derive(Debug)]
pub(crate) struct SubscribeLoopParams<TTransport> {
    pub control_rx: ControlRx,
    pub ready_tx: Option<ReadyTx>,
    pub exit_tx: Option<ExitTx>,

    pub transport: TTransport,

    pub channels: Registry,
    pub channel_groups: Registry,
}

/// Implements the subscribe loop, which efficiently polls for new messages.
pub(crate) async fn subscribe_loop<TTransport>(params: SubscribeLoopParams<TTransport>)
where
    TTransport: Transport,
    <TTransport as Transport>::Error: 'static,
{
    debug!("Starting subscribe loop");

    #[allow(clippy::unneeded_field_pattern)]
    let SubscribeLoopParams {
        mut control_rx,
        mut ready_tx,
        mut exit_tx,

        transport,

        mut channels,
        mut channel_groups,
    } = params;

    let mut timetoken = Timetoken::default();

    loop {
        // TODO: re-add cache.
        let channels_list: Vec<String> = channels.keys().cloned().collect();
        let request = request::SubscribeV2 {
            channels: channels_list,
            timetoken: timetoken.clone(),
        };
        let response = transport.subscribe_request_v2(request);

        let response = response.fuse();
        futures_util::pin_mut!(response);

        let control_rx_recv = control_rx.next();
        futures_util::pin_mut!(control_rx_recv);

        let (messages, next_timetoken) = match select(control_rx_recv, response).await {
            Either::Left((msg, _)) => {
                let outcome = handle_control_command(&mut channels, &mut channel_groups, msg).await;
                if let ControlOutcome::Terminate = outcome {
                    // Termination requested, break the loop.
                    break;
                }

                // Control signalled we can continue with the polling, however
                // we literally need to `continue` here in order to force rerun
                // the loop from the beginning.
                // We rely on the in-flight request to be properly cleaned up,
                // since their futures are being dropped here.
                continue;
            }
            Either::Right((res, _)) => {
                match res {
                    Ok(v) => v,
                    Err(err) => {
                        // TODO: add some kind of circut breaker.
                        // Report error and retry - maybe it'd work this time.
                        error!("Transport error while polling: {:?}", err);
                        continue;
                    }
                }
            }
        };

        // Send ready message when the subscribe loop is capable of receiving
        // messages.
        // This is intended to signal the readiness (and the healthiness) of
        // the setup. It is invoked after the `Ok` result from the request
        // future, guaranteing that Transport was able to perform successfully
        // at least once.
        if let Some(ready_tx) = ready_tx.take() {
            if let Err(err) = ready_tx.send(()) {
                error!("Error sending ready message: {:?}", err);
                break;
            }
        }

        // Save Timetoken for next request
        timetoken = next_timetoken;

        debug!("messages: {:?}", messages);
        debug!("timetoken: {:?}", timetoken);

        // Distribute messages to each listener.
        for message in messages {
            let route = message
                .route
                .clone()
                .unwrap_or_else(|| message.channel.clone());
            debug!("route: {}", route);

            // TODO: provide a better interface and remove the potentially
            // unsound `get` and `get_mut` from the registry API.
            let listeners = channels.get_mut(&route).unwrap();

            debug!("Delivering to {} listeners...", listeners.len());
            for channel_tx in listeners.iter_mut() {
                if let Err(error) = channel_tx.send(message.clone()).await {
                    error!("Delivery error: {:?}", error);
                }
            }
        }
    }

    debug!("Stopping subscribe loop");

    if let Some(ref mut exit_tx) = exit_tx {
        exit_tx.send(()).await.expect("Unable to send exit message");
    }
}

/// Encodes action to be taken in response to control command.
#[derive(Debug)]
enum ControlOutcome {
    Terminate,
    CanContinue,
}

/// Handle a control command.
async fn handle_control_command(
    channels: &mut Registry,
    channel_groups: &mut Registry,
    msg: Option<ControlCommand>,
) -> ControlOutcome {
    debug!("Got request: {:?}", msg);
    let request = match msg {
        Some(v) => v,
        None => return ControlOutcome::CanContinue,
    };
    match request {
        ControlCommand::Drop(id, listener) => {
            // Remove channel or group listener.
            let (name, kind, registry, other_is_empty) = match listener {
                ListenerType::Channel(name) => {
                    (name, "channel", channels, channel_groups.is_empty())
                }
                ListenerType::ChannelGroup(name) => {
                    (name, "group", channel_groups, channels.is_empty())
                }
            };

            // Unregister the listener from the registry.
            debug!("Removing {} from SubscribeLoop: {}", kind, name);

            let (_, _effect) = registry
                .unregister(&name, id)
                .expect("Unable to get channel listeners");

            // TODO: avoid terminating loop here to avoid special casing.
            if other_is_empty && registry.is_empty() {
                ControlOutcome::Terminate
            } else {
                ControlOutcome::CanContinue
            }
        }
        ControlCommand::Add(listener, channel_tx, id_tx) => {
            // Add channel or group listener.
            let (name, kind, registry) = match listener {
                ListenerType::Channel(name) => (name, "channel", channels),
                ListenerType::ChannelGroup(name) => (name, "group", channel_groups),
            };
            debug!("Adding {} to SubscribeLoop: {}", kind, name);

            // Register new channel listener with the registry.
            let (id, _effect) = registry.register(name, channel_tx);

            // Send Subscription ID.
            id_tx.send(id).expect("Unable to send subscription id");

            ControlOutcome::CanContinue
        }
    }
}