use std::{
    io::{Error as IoError, ErrorKind, Result as IoResult},
    sync::Arc,
};

use base64::URL_SAFE_NO_PAD;
use http::{header, Uri};
use hyper::{client::HttpConnector, Client};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};

use crate::{
    listener::acme::{
        jose,
        keypair::KeyPair,
        protocol::{
            CsrRequest, Directory, FetchAuthorizationResponse, Identifier, NewAccountRequest,
            NewOrderRequest, NewOrderResponse,
        },
    },
    Body,
};

pub(crate) struct AcmeClient {
    client: Client<HttpsConnector<HttpConnector>>,
    directory: Directory,
    key_pair: Arc<KeyPair>,
    kid: String,
}

impl AcmeClient {
    pub(crate) async fn try_new(directory_url: &Uri, key_pair: Arc<KeyPair>) -> IoResult<Self> {
        let client = Client::builder().build(
            HttpsConnectorBuilder::new()
                .with_native_roots()
                .https_or_http()
                .enable_http1()
                .build(),
        );
        let directory = get_directory(&client, directory_url).await?;
        let kid = create_acme_account(&client, &directory, &key_pair).await?;
        Ok(Self {
            client,
            directory,
            key_pair,
            kid,
        })
    }

    pub(crate) async fn new_order(&self, domains: &[String]) -> IoResult<NewOrderResponse> {
        tracing::debug!(kid = self.kid.as_str(), "new order request");

        let nonce = get_nonce(&self.client, &self.directory).await?;
        let resp: NewOrderResponse = jose::request_json(
            &self.client,
            &self.key_pair,
            Some(&self.kid),
            &nonce,
            &self.directory.new_order,
            Some(NewOrderRequest {
                identifiers: domains
                    .iter()
                    .map(|domain| Identifier {
                        ty: "dns".to_string(),
                        value: domain.to_string(),
                    })
                    .collect(),
            }),
        )
        .await?;

        tracing::debug!(status = resp.status.as_str(), "order created");
        Ok(resp)
    }

    pub(crate) async fn fetch_authorization(
        &self,
        auth_url: &Uri,
    ) -> IoResult<FetchAuthorizationResponse> {
        tracing::debug!(auth_uri = %auth_url, "fetch authorization");

        let nonce = get_nonce(&self.client, &self.directory).await?;
        let resp: FetchAuthorizationResponse = jose::request_json(
            &self.client,
            &self.key_pair,
            Some(&self.kid),
            &nonce,
            auth_url,
            None::<()>,
        )
        .await?;

        tracing::debug!(
            identifier = ?resp.identifier,
            status = resp.status.as_str(),
            "authorization response",
        );

        Ok(resp)
    }

    pub(crate) async fn trigger_challenge(&self, domain: &str, url: &Uri) -> IoResult<()> {
        tracing::debug!(auth_uri = %url, domain = domain, "trigger challenge");

        let nonce = get_nonce(&self.client, &self.directory).await?;
        jose::request(
            &self.client,
            &self.key_pair,
            Some(&self.kid),
            &nonce,
            url,
            Some(serde_json::json!({})),
        )
        .await?;

        Ok(())
    }

    pub(crate) async fn send_csr(&self, url: &Uri, csr: &[u8]) -> IoResult<NewOrderResponse> {
        tracing::debug!(url = %url, "send certificate request");

        let nonce = get_nonce(&self.client, &self.directory).await?;
        jose::request_json(
            &self.client,
            &self.key_pair,
            Some(&self.kid),
            &nonce,
            url,
            Some(CsrRequest {
                csr: base64::encode_config(csr, URL_SAFE_NO_PAD),
            }),
        )
        .await
    }

    pub(crate) async fn obtain_certificate(&self, url: &Uri) -> IoResult<Vec<u8>> {
        tracing::debug!(url = %url, "send certificate request");

        let nonce = get_nonce(&self.client, &self.directory).await?;
        let resp = jose::request(
            &self.client,
            &self.key_pair,
            Some(&self.kid),
            &nonce,
            url,
            None::<()>,
        )
        .await?;

        resp.into_body().into_vec().await.map_err(|err| {
            IoError::new(
                ErrorKind::Other,
                format!("failed to download certificate: {}", err),
            )
        })
    }
}

async fn get_directory(
    client: &Client<HttpsConnector<HttpConnector>>,
    directory_url: &Uri,
) -> IoResult<Directory> {
    tracing::debug!("loading directory");

    let resp = client.get(directory_url.clone()).await.map_err(|err| {
        IoError::new(
            ErrorKind::Other,
            format!("failed to load directory: {}", err),
        )
    })?;

    if !resp.status().is_success() {
        return Err(IoError::new(
            ErrorKind::Other,
            format!("failed to load directory: status = {}", resp.status()),
        ));
    }

    let directory = Body(resp.into_body())
        .into_json::<Directory>()
        .await
        .map_err(|err| {
            IoError::new(
                ErrorKind::Other,
                format!("failed to load directory: {}", err),
            )
        })?;

    tracing::debug!(
        new_nonce = ?directory.new_nonce,
        new_account = ?directory.new_account,
        new_order = ?directory.new_order,
        "directory loaded",
    );
    Ok(directory)
}

async fn get_nonce(
    client: &Client<HttpsConnector<HttpConnector>>,
    directory: &Directory,
) -> IoResult<String> {
    tracing::debug!("creating nonce");

    let resp = client
        .get(directory.new_nonce.clone())
        .await
        .map_err(|err| IoError::new(ErrorKind::Other, format!("failed to get nonce: {}", err)))?;

    if !resp.status().is_success() {
        return Err(IoError::new(
            ErrorKind::Other,
            format!("failed to load directory: status = {}", resp.status()),
        ));
    }

    let nonce = resp
        .headers()
        .get("replay-nonce")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
        .unwrap_or_default();

    tracing::debug!(nonce = nonce.as_str(), "nonce created");
    Ok(nonce)
}

async fn create_acme_account(
    client: &Client<HttpsConnector<HttpConnector>>,
    directory: &Directory,
    key_pair: &KeyPair,
) -> IoResult<String> {
    tracing::debug!("creating acme account");

    let nonce = get_nonce(client, directory).await?;
    let resp = jose::request(
        client,
        key_pair,
        None,
        &nonce,
        &directory.new_account,
        Some(NewAccountRequest {
            only_return_existing: false,
            terms_of_service_agreed: true,
            contact: vec![],
        }),
    )
    .await?;
    let kid = resp
        .header(header::LOCATION)
        .ok_or_else(|| IoError::new(ErrorKind::Other, "unable to get account id"))?
        .to_string();

    tracing::debug!(kid = kid.as_str(), "account created");
    Ok(kid)
}