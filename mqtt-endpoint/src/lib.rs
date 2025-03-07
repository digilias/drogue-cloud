mod auth;
mod config;
mod service;

pub use config::Config;

use crate::{auth::DeviceAuthenticator, service::App};
use drogue_cloud_endpoint_common::{
    command::{Commands, KafkaCommandSource},
    sender::DownstreamSender,
    sink::KafkaSink,
};
use drogue_cloud_mqtt_common::server::build;
use drogue_cloud_service_common::{health::HealthServer, metrics};
use futures::TryFutureExt;
use lazy_static::lazy_static;
use prometheus::{IntGauge, Opts};

lazy_static! {
    pub static ref CONNECTIONS_COUNTER: IntGauge = IntGauge::with_opts(
        Opts::new("drogue_connections", "Connections")
            .const_label("protocol", "mqtt")
            .const_label("type", "endpoint")
    )
    .unwrap();
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let commands = Commands::new();

    let app = App {
        config: config.endpoint.clone(),
        downstream: DownstreamSender::new(
            KafkaSink::from_config(
                config.kafka_downstream_config.clone(),
                config.check_kafka_topic_ready,
            )?,
            config.instance.clone(),
            config.endpoint_pool.clone(),
        )?,

        authenticator: DeviceAuthenticator(
            drogue_cloud_endpoint_common::auth::DeviceAuthenticator::new(config.auth.clone())
                .await?,
        ),
        commands: commands.clone(),
    };

    let srv = build(config.mqtt.clone(), app, &config)?.run();

    log::info!("Starting web server");

    // command source

    let command_source = KafkaCommandSource::new(
        commands,
        config.kafka_command_config,
        config.command_source_kafka,
    )?;

    // run
    if let Some(health) = config.health {
        metrics::register(Box::new(CONNECTIONS_COUNTER.clone()))?;
        // health server
        let health = HealthServer::new(
            health,
            vec![Box::new(command_source)],
            Some(prometheus::default_registry().clone()),
        );
        futures::try_join!(health.run_ntex(), srv.err_into(),)?;
    } else {
        futures::try_join!(srv)?;
    }

    // exiting

    Ok(())
}
