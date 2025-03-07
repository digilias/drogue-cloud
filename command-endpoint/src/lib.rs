mod v1alpha1;

use actix_cors::Cors;
use actix_web::{middleware, web, App, HttpResponse, HttpServer, Responder};
use drogue_client::openid::OpenIdTokenProvider;
use drogue_cloud_endpoint_common::sender::ExternalClientPoolConfig;
use drogue_cloud_endpoint_common::{sender::UpstreamSender, sink::KafkaSink};
use drogue_cloud_service_api::webapp as actix_web;
use drogue_cloud_service_api::{auth::user::authz::Permission, kafka::KafkaClientConfig};
use drogue_cloud_service_common::{
    actix_auth::authentication::AuthN,
    actix_auth::authorization::AuthZ,
    client::{RegistryConfig, UserAuthClient, UserAuthClientConfig},
    openid::AuthenticatorConfig,
};
use drogue_cloud_service_common::{
    defaults,
    health::{HealthServer, HealthServerConfig},
    openid::Authenticator,
};
use futures::TryFutureExt;
use serde::Deserialize;
use serde_json::json;
use std::str;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[serde(default = "defaults::max_json_payload_size")]
    pub max_json_payload_size: usize,
    #[serde(default = "defaults::bind_addr")]
    pub bind_addr: String,
    #[serde(default = "defaults::enable_access_token")]
    pub enable_access_token: bool,

    pub registry: RegistryConfig,

    #[serde(default)]
    pub health: Option<HealthServerConfig>,

    #[serde(default)]
    pub user_auth: Option<UserAuthClientConfig>,

    pub oauth: AuthenticatorConfig,

    pub command_kafka_sink: KafkaClientConfig,

    #[serde(default = "defaults::check_kafka_topic_ready")]
    pub check_kafka_topic_ready: bool,

    #[serde(default = "defaults::instance")]
    pub instance: String,

    #[serde(default)]
    pub endpoint_pool: ExternalClientPoolConfig,
}

#[derive(Debug)]
pub struct WebData {
    pub authenticator: Option<Authenticator>,
}

async fn index() -> impl Responder {
    HttpResponse::Ok().json(json!({"success": true}))
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    log::info!("Starting Command service endpoint");

    let sender = UpstreamSender::new(
        config.instance,
        KafkaSink::from_config(config.command_kafka_sink, config.check_kafka_topic_ready)?,
        config.endpoint_pool,
    )?;

    let max_json_payload_size = config.max_json_payload_size;

    let enable_access_token = config.enable_access_token;

    // set up authentication

    let authenticator = config.oauth.into_client().await?;
    let user_auth = if let Some(user_auth) = config.user_auth {
        let user_auth = UserAuthClient::from_config(user_auth).await?;
        Some(user_auth)
    } else {
        None
    };

    let client = reqwest::Client::new();
    let registry = config.registry.into_client().await?;

    // main server

    let main = HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::default())
            .app_data(web::JsonConfig::default().limit(max_json_payload_size))
            .app_data(web::Data::new(sender.clone()))
            .app_data(web::Data::new(registry.clone()))
            .app_data(web::Data::new(client.clone()))
            .service(web::resource("/").route(web::get().to(index)))
            .service(
                web::scope("/api/command/v1alpha1/apps/{application}/devices/{deviceId}")
                    .wrap(AuthZ {
                        client: user_auth.clone(),
                        permission: Permission::Write,
                        app_param: "application".to_string(),
                    })
                    .wrap(AuthN {
                        openid: authenticator.as_ref().cloned(),
                        token: user_auth.clone(),
                        enable_access_token,
                    })
                    .wrap(Cors::permissive())
                    .route(
                        "",
                        web::post().to(v1alpha1::command::<KafkaSink, Option<OpenIdTokenProvider>>),
                    ),
            )
    })
    .bind(config.bind_addr)?
    .run();

    // run

    if let Some(health) = config.health {
        let health =
            HealthServer::new(health, vec![], Some(prometheus::default_registry().clone()));
        futures::try_join!(health.run(), main.err_into())?;
    } else {
        futures::try_join!(main)?;
    }

    // exiting

    Ok(())
}
