use std::net::SocketAddr;

use crate::command::wait_for_command;
use crate::error::CoapEndpointError;
use async_trait::async_trait;
use coap_lite::{CoapRequest, CoapResponse, ResponseType};
use drogue_cloud_endpoint_common::downstream::Publisher;
use drogue_cloud_endpoint_common::{
    command::Commands,
    downstream::{DownstreamSender, Publish, PublishOutcome},
    error::EndpointError,
    sink::Sink,
};
use drogue_cloud_service_common::Id;

#[async_trait]
pub trait CoapCommandSender {
    async fn publish_and_await<'a, B>(
        &self,
        publish: Publish<'a>,
        commands: Commands,
        ttd: Option<u64>,
        body: B,
        req: CoapRequest<SocketAddr>,
    ) -> Result<Option<CoapResponse>, CoapEndpointError>
    where
        B: AsRef<[u8]> + Send + Sync;
}

#[async_trait]
impl<S> CoapCommandSender for DownstreamSender<S>
where
    S: Sink + Send + Sync,
    <S as Sink>::Error: Send,
{
    async fn publish_and_await<'a, B>(
        &self,
        publish: Publish<'a>,
        commands: Commands,
        ttd: Option<u64>,
        body: B,
        req: CoapRequest<SocketAddr>,
    ) -> Result<Option<CoapResponse>, CoapEndpointError>
    where
        B: AsRef<[u8]> + Send + Sync,
    {
        let id = Id::new(&publish.application.metadata.name, &publish.device_id);
        match self.publish(publish, body).await {
            // ok, and accepted
            Ok(PublishOutcome::Accepted) => wait_for_command(req, commands, id, ttd).await,

            // ok, but rejected
            Ok(PublishOutcome::Rejected) => Ok(req.response.map(|mut v| {
                v.set_status(ResponseType::NotAcceptable);
                v
            })),

            // ok, but queue full
            Ok(PublishOutcome::QueueFull) => Ok(req.response.map(|mut v| {
                v.set_status(ResponseType::ServiceUnavailable);
                v
            })),

            // internal error
            Err(err) => Err(CoapEndpointError(EndpointError::ConfigurationError {
                details: err.to_string(),
            })),
        }
    }
}
