mod auth;
mod endpoints;
mod error;
mod info;
mod kube;
mod spy;

use crate::auth::Authenticator;
use crate::endpoints::{
    EndpointSourceType, EnvEndpointSource, KubernetesEndpointSource, OpenshiftEndpointSource,
};
use crate::error::ServiceError;
use actix_cors::Cors;
use actix_web::{
    get,
    middleware::{self, Condition},
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use actix_web_httpauth::middleware::HttpAuthentication;
use anyhow::Context;
use envconfig::Envconfig;
use failure::Fail;
use reqwest::Certificate;
use serde_json::json;
use std::{fs::File, io::Read, path::Path};
use url::Url;

#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok().json(json!({"success": true}))
}

// TODO: move to a different port
#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().finish()
}

#[derive(Debug, Envconfig)]
struct Config {
    #[envconfig(from = "BIND_ADDR")]
    pub bind_addr: Option<String>,
    #[envconfig(from = "ENABLE_AUTH")]
    pub enable_auth: bool,
    #[envconfig(from = "CLIENT_ID")]
    pub client_id: Option<String>,
    #[envconfig(from = "CLIENT_SECRET")]
    pub client_secret: Option<String>,
    #[envconfig(from = "ISSUER_URL")]
    pub issuer_url: Option<String>,
    #[envconfig(from = "REDIRECT_URL")]
    pub redirect_url: Option<String>,
    // "drogue" is the client id which is required for the "aud" claim
    #[envconfig(from = "SCOPES", default = "openid profile email drogue")]
    pub scopes: String,
}

const SERVICE_CA_CERT: &str = "/var/run/secrets/kubernetes.io/serviceaccount/service-ca.crt";

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config = Config::init_from_env()?;

    // the endpoint source we choose
    let endpoint_source = create_endpoint_source()?;
    log::info!("Using endpoint source: {:?}", endpoint_source);
    let endpoint_source: Data<EndpointSourceType> = Data::new(endpoint_source);

    // OpenIdConnect

    let enable_auth = config.enable_auth;

    let client = if enable_auth {
        let mut client = reqwest::ClientBuilder::new();
        let redirect_url = config
            .redirect_url
            .expect("Missing 'REDIRECT_URL' variable");

        let cert = Path::new(SERVICE_CA_CERT);
        if cert.exists() {
            log::info!("Adding root certificate: {}", SERVICE_CA_CERT);
            let mut file = File::open(cert)?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;

            let pems = pem::parse_many(buf);
            let pems = pems
                .into_iter()
                .map(|pem| {
                    Certificate::from_pem(&pem::encode(&pem).into_bytes()).map_err(|err| err.into())
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            log::info!("Found {} certificates", pems.len());

            for pem in pems {
                log::info!("Adding root certificate: {:?}", pem);
                client = client.add_root_certificate(pem);
            }
        } else {
            log::info!(
                "Service CA certificate does not exist, skipping! ({})",
                SERVICE_CA_CERT
            );
        }

        let client = openid::DiscoveredClient::discover_with_client(
            client.build()?,
            config
                .client_id
                .ok_or_else(|| anyhow::anyhow!("Missing 'CLIENT_ID' variable"))?,
            config
                .client_secret
                .ok_or_else(|| anyhow::anyhow!("Missing 'CLIENT_SECRET' variable"))?,
            Some(redirect_url),
            config
                .issuer_url
                .ok_or_else(|| anyhow::anyhow!("Missing 'ISSUER_URL' variable"))
                .and_then(|url| {
                    Url::parse(&url).with_context(|| format!("Failed to parse issuer URL: {}", url))
                })?,
        )
        .await
        .map_err(|err| anyhow::Error::from(err.compat()))?;

        log::info!("Discovered OpenID: {:#?}", client.config());

        Some(client)
    } else {
        None
    };

    let authenticator = web::Data::new(auth::Authenticator {
        client,
        scopes: config.scopes,
    });

    // http server

    HttpServer::new(move || {
        let auth = HttpAuthentication::bearer(|req, auth| {
            let token = auth.token().to_string();

            async {
                let authenticator = req.app_data::<web::Data<Authenticator>>();
                log::info!("Authenticator: {:?}", &authenticator);
                let authenticator = authenticator.ok_or_else(|| ServiceError::InternalError {
                    message: "Missing authenticator instance".into(),
                })?;

                authenticator.validate_token(token).await?;
                Ok(req)
            }
        });

        App::new()
            .wrap(middleware::Logger::default())
            .wrap(Cors::permissive().supports_credentials())
            .data(web::JsonConfig::default().limit(4096))
            .app_data(authenticator.clone())
            .app_data(endpoint_source.clone())
            .service(
                web::scope("/api/v1")
                    .wrap(Condition::new(enable_auth, auth))
                    .service(info::get_info),
            )
            .service(spy::stream_events) // this one is special, SSE doesn't support authorization headers
            .service(index)
            .service(health)
            .service(auth::login)
            .service(auth::code)
    })
    .bind(config.bind_addr.unwrap_or_else(|| "127.0.0.1:8080".into()))?
    .run()
    .await?;

    Ok(())
}

fn create_endpoint_source() -> anyhow::Result<EndpointSourceType> {
    let endpoints: endpoints::EndpointConfig = Envconfig::init_from_env()?;
    match endpoints.source.as_str() {
        "openshift" => Ok(Box::new(OpenshiftEndpointSource::new()?)),
        "kubernetes" => Ok(Box::new(KubernetesEndpointSource::new()?)),
        "env" => Ok(Box::new(EnvEndpointSource(endpoints))),
        other => Err(anyhow::anyhow!("Unsupported endpoint source: '{}'", other)),
    }
}
