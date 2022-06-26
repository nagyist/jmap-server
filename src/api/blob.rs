use std::collections::HashMap;

use super::{RequestError, RequestLimitError};
use crate::authorization::auth::RemoteAddress;
use crate::authorization::Session;
use crate::JMAPServer;
use actix_web::http::header::ContentType;
use actix_web::HttpRequest;
use actix_web::{http::StatusCode, web, HttpResponse};
use jmap::error::set::SetError;
use jmap::request::blob::{CopyBlobRequest, CopyBlobResponse};
use jmap::request::ACLEnforce;
use jmap::types::blob::JMAPBlob;
use jmap::types::jmap::JMAPId;
use jmap::SUPERUSER_ID;
use jmap_mail::mail::get::{BlobResult, JMAPGetMail};
use jmap_mail::mail::sharing::JMAPShareMail;
use jmap_sharing::principal::account::JMAPAccountStore;
use reqwest::header::CONTENT_TYPE;
use store::core::acl::ACL;
use store::core::collection::Collection;
use store::JMAPStore;
use store::{tracing::error, Store};

#[derive(serde::Deserialize)]
pub struct Params {
    accept: String,
}

pub async fn handle_jmap_download<T>(
    path: web::Path<(JMAPId, JMAPBlob, String)>,
    params: web::Query<Params>,
    core: web::Data<JMAPServer<T>>,
    session: Session,
) -> Result<HttpResponse, RequestError>
where
    T: for<'x> Store<'x> + 'static,
{
    // Enforce access control
    let (id, blob_id, filename) = path.into_inner();
    let account_id = id.get_document_id();

    let store = core.store.clone();
    match core
        .spawn_worker(move || {
            store.mail_blob_get(
                account_id,
                &store.get_acl_token(session.account_id())?,
                &blob_id,
            )
        })
        .await
    {
        Ok(BlobResult::Blob(bytes)) => {
            Ok(HttpResponse::build(StatusCode::OK)
                .insert_header(("Content-Type", params.into_inner().accept))
                .insert_header((
                    "Content-Disposition",
                    format!("attachment; filename=\"{}\"", filename), //TODO escape filename
                ))
                .insert_header(("Cache-Control", "private, immutable, max-age=31536000"))
                .body(bytes))
        }
        Ok(BlobResult::NotFound) => Err(RequestError::not_found()),
        Ok(BlobResult::Unauthorized) => Err(RequestError::forbidden()),
        Err(err) => {
            error!("Blob download failed: {:?}", err);
            Err(RequestError::internal_server_error())
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct UploadResponse {
    #[serde(rename(serialize = "accountId"))]
    account_id: JMAPId,
    #[serde(rename(serialize = "blobId"))]
    blob_id: JMAPBlob,
    #[serde(rename(serialize = "type"))]
    c_type: String,
    size: usize,
}

pub async fn handle_jmap_upload<T>(
    path: web::Path<(JMAPId,)>,
    request: HttpRequest,
    bytes: web::Bytes,
    core: web::Data<JMAPServer<T>>,
    session: Session,
) -> Result<HttpResponse, RequestError>
where
    T: for<'x> Store<'x> + 'static,
{
    let (id,) = path.into_inner();
    let account_id = id.get_document_id();

    // Rate limit uploads
    let _upload_req = core
        .rate_limiters
        .get(&RemoteAddress::AccountId(session.account_id()))
        .unwrap()
        .is_upload_allowed(core.store.config.max_concurrent_upload)
        .ok_or_else(|| RequestError::limit(RequestLimitError::Concurrent))?;

    #[cfg(test)]
    {
        // Used for concurrent upload tests
        if bytes == b"sleep"[..] {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    let store = core.store.clone();
    let size = bytes.len();
    match core
        .spawn_worker(move || {
            Ok(
                if store
                    .get_acl_token(session.account_id())?
                    .is_member(account_id)
                {
                    let blob_id = store.blob_store(&bytes)?;
                    store.blob_link_ephimeral(&blob_id, account_id)?;
                    JMAPBlob::new(blob_id).into()
                } else {
                    None
                },
            )
        })
        .await
    {
        Ok(Some(blob_id)) => Ok(HttpResponse::build(StatusCode::OK)
            .insert_header(ContentType::json())
            .json(UploadResponse {
                account_id: id,
                blob_id,
                c_type: request
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                size,
            })),
        Ok(None) => Err(RequestError::forbidden()),
        Err(err) => {
            error!("Blob upload failed: {:?}", err);
            Err(RequestError::internal_server_error())
        }
    }
}

pub trait JMAPBlobCopy<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn copy_blob(&self, request: CopyBlobRequest) -> jmap::Result<CopyBlobResponse>;
}

impl<T> JMAPBlobCopy<T> for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn copy_blob(&self, request: CopyBlobRequest) -> jmap::Result<CopyBlobResponse> {
        let acl = request.acl.unwrap();
        let account_id = request.account_id.get_document_id();
        let from_account_id = request.from_account_id.get_document_id();
        let mut copied = HashMap::with_capacity(request.blob_ids.len());
        let mut not_copied = HashMap::new();

        for blob_id in request.blob_ids {
            if !self.blob_account_has_access(&blob_id.id, &acl.member_of)?
                && !acl.is_member(SUPERUSER_ID)
            {
                if let Some(shared_ids) = self
                    .mail_shared_messages(from_account_id, &acl.member_of, ACL::ReadItems)?
                    .as_ref()
                {
                    if !self.blob_document_has_access(
                        &blob_id.id,
                        from_account_id,
                        Collection::Mail,
                        shared_ids,
                    )? {
                        not_copied.insert(
                            blob_id,
                            SetError::forbidden("You do not have access to this blobId."),
                        );
                        continue;
                    }
                } else {
                    not_copied.insert(
                        blob_id,
                        SetError::forbidden("You do not have access to this blobId."),
                    );
                    continue;
                }
            }
            self.blob_link_ephimeral(&blob_id.id, account_id)?;
            copied.insert(blob_id.clone(), blob_id);
        }

        Ok(CopyBlobResponse {
            from_account_id: request.from_account_id,
            account_id: request.account_id,
            copied: if !copied.is_empty() {
                copied.into()
            } else {
                None
            },
            not_copied: if !not_copied.is_empty() {
                not_copied.into()
            } else {
                None
            },
        })
    }
}
