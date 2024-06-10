// Copyright 2024 Textile
// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use std::{convert::Infallible, net::ToSocketAddrs, num::ParseIntError};

use anyhow::anyhow;
use async_tempfile::TempFile;
use base64::{engine::general_purpose, Engine};
use bytes::Buf;
use cid::Cid;
use ethers::core::types::{self as et};
use fendermint_actor_objectstore::{Object, ObjectList, ObjectListItem};
use fendermint_rpc::QueryClient;
use fendermint_vm_message::conv::from_fvm::to_eth_tokens;
use fendermint_vm_message::signed::SignedMessage;
use futures_util::StreamExt;
use fvm_shared::{address::Address, econ::TokenAmount};
use ipfs_api_backend_hyper::request::Add;
use ipfs_api_backend_hyper::{IpfsApi, IpfsClient, TryFromUri};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::compat::TokioAsyncReadCompatExt;
use warp::{
    filters::multipart::Part,
    http::{HeaderMap, HeaderValue, StatusCode},
    hyper::body::Body,
    path::Tail,
    Filter, Rejection, Reply,
};

use fendermint_actor_objectstore::{GetParams, ListParams};
use fendermint_app_settings::objects::ObjectsSettings;
use fendermint_rpc::{client::FendermintClient, tx::CallClient};
use fendermint_vm_message::query::FvmQueryHeight;
use fvm_shared::chainid::ChainID;

use super::rpc::{gas_params, TransClient};
use crate::cmd;
use crate::options::{
    objects::{ObjectsArgs, ObjectsCommands},
    rpc::TransArgs,
};

const MAX_OBJECT_LENGTH: u64 = 1024 * 1024 * 1024;

cmd! {
    ObjectsArgs(self, settings: ObjectsSettings) {
        match self.command.clone() {
            ObjectsCommands::Run { tendermint_url, ipfs_addr, args} => {
                let client = FendermintClient::new_http(tendermint_url, None)?;
                let ipfs = IpfsClient::from_multiaddr_str(&ipfs_addr)?;
                let ipfs_adapter = Ipfs { inner: ipfs.clone() };

                // Admin routes
                let health_route = warp::path!("health")
                    .and(warp::get()).and_then(health);

                // Objects routes
                let objects_upload = warp::path!("v1" / "objects" )
                .and(warp::post())
                .and(with_client(client.clone()))
                .and(with_ipfs_adapter(ipfs_adapter.clone()))
                .and(warp::multipart::form().max_length(MAX_OBJECT_LENGTH))
                .and_then(handle_object_upload);

                let objects_download = warp::path!("v1" / "objects" / Address / String)
                .and(
                    warp::get().map(|| "GET".to_string()).or(warp::head().map(|| "HEAD".to_string())).unify()

                )
                .and(warp::header::optional::<String>("Range"))
                .and(warp::query::<HeightQuery>())
                .and(with_client(client.clone()))
                .and(with_ipfs(ipfs.clone()))
                .and(with_args(args.clone()))
                .and_then(handle_object_download);

                // TODO: Deprecated, remove after SDK migration
                let os_get_or_list_route = warp::path!("v1" / "objectstores" / Address / ..)
                    .and(
                        warp::get().or(warp::head()).unify()
                    )
                    .and(warp::path::tail())
                    .and(warp::query::<HeightQuery>())
                    .and(warp::query::<ListQuery>())
                    .and(warp::header::optional::<String>("Range"))
                    .and(with_client(client.clone()))
                    .and(with_ipfs(ipfs.clone()))
                    .and(with_args(args.clone()))
                    .and_then(handle_object_download_deprecated);

                let router = health_route
                    .or(objects_upload)
                    .or(objects_download)
                    .or(os_get_or_list_route)
                    .with(warp::cors().allow_any_origin()
                        .allow_headers(vec!["Content-Type"])
                        .allow_methods(vec!["PUT", "DEL", "GET", "HEAD"]))
                    .recover(handle_rejection);

                if let Some(listen_addr) = settings.listen.to_socket_addrs()?.next() {
                    warp::serve(router).run(listen_addr).await;
                    Ok(())
                } else {
                    Err(anyhow!("failed to convert to any socket address"))
                }
            },
        }
    }
}

fn with_client(
    client: FendermintClient,
) -> impl Filter<Extract = (FendermintClient,), Error = Infallible> + Clone {
    warp::any().map(move || client.clone())
}

fn with_ipfs(
    client: IpfsClient,
) -> impl Filter<Extract = (IpfsClient,), Error = Infallible> + Clone {
    warp::any().map(move || client.clone())
}

fn with_ipfs_adapter<I: IpfsApiAdapter + Clone + Send>(
    client: I,
) -> impl Filter<Extract = (I,), Error = Infallible> + Clone {
    warp::any().map(move || client.clone())
}

fn with_args(args: TransArgs) -> impl Filter<Extract = (TransArgs,), Error = Infallible> + Clone {
    warp::any().map(move || args.clone())
}

#[derive(Serialize, Deserialize)]
struct HeightQuery {
    pub height: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct ListQuery {
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Debug, Error)]
enum ObjectsError {
    #[error("error parsing range header: `{0}`")]
    RangeHeaderParseError(ParseIntError),
    #[error("invalid range header")]
    RangeHeaderInvalid,
}

impl From<ParseIntError> for ObjectsError {
    fn from(err: ParseIntError) -> Self {
        ObjectsError::RangeHeaderParseError(err)
    }
}

pub trait IpfsApiAdapter {
    async fn add_file(&self, temp_file: TempFile, cid: Cid) -> anyhow::Result<String>;
}

#[derive(Clone)]
pub struct Ipfs {
    inner: IpfsClient,
}

impl IpfsApiAdapter for Ipfs {
    async fn add_file(&self, mut temp_file: TempFile, cid_from_msg: Cid) -> anyhow::Result<String> {
        let temp_file_clone = temp_file.try_clone().await?;
        // Only chunk and hash - do not write to disk
        let res = self
            .inner
            .add_async_with_options(
                temp_file_clone.compat(),
                Add {
                    chunker: Some("size-1048576"),
                    raw_leaves: Some(false),
                    pin: Some(false),
                    cid_version: Some(1),
                    only_hash: Some(true),
                    ..Default::default()
                },
            )
            .await?;

        // Check if the computed CID matches the one in the signed message
        // It is important to verify that CID represents the data correctly
        // separately from signature because the signature is over the CID,
        // it is unaware of the actual data.
        let ipfs_cid = Cid::try_from(res.hash)?;
        if ipfs_cid != cid_from_msg {
            return Err(anyhow!(
                "computed cid {:?} does not match {:?}",
                ipfs_cid,
                cid_from_msg
            ));
        }

        // Actually add the file to IPFS
        temp_file.rewind().await?;
        let res = self
            .inner
            .add_async_with_options(
                temp_file.compat(),
                Add {
                    chunker: Some("size-1048576"),
                    raw_leaves: Some(false),
                    pin: Some(false),
                    cid_version: Some(1),
                    ..Default::default()
                },
            )
            .await?;
        let cid = Cid::try_from(res.hash)?;
        Ok(cid.to_string())
    }
}

struct ObjectParser {
    signed_msg: Option<SignedMessage>,
    chain_id: ChainID,
    temp_file: Option<TempFile>,
}

impl Default for ObjectParser {
    fn default() -> Self {
        ObjectParser {
            signed_msg: None,
            chain_id: ChainID::from(0),
            temp_file: None,
        }
    }
}

impl ObjectParser {
    async fn read_part(&mut self, part: Part) -> anyhow::Result<Vec<u8>> {
        let value = part
            .stream()
            .fold(Vec::new(), |mut vec, data| async move {
                if let Ok(data) = data {
                    vec.extend_from_slice(data.chunk());
                }
                vec
            })
            .await;
        Ok(value)
    }

    async fn read_chain_id(&mut self, form_part: Part) -> anyhow::Result<()> {
        let value = self.read_part(form_part).await?;
        let text = String::from_utf8(value).map_err(|_| anyhow!("cannot parse chain id"))?;
        let int: u64 = text.parse().map_err(|_| anyhow!("cannot parse chain_id"))?;
        self.chain_id = ChainID::from(int);
        Ok(())
    }

    async fn read_msg(&mut self, form_part: Part) -> anyhow::Result<()> {
        let value = self.read_part(form_part).await?;
        let signed_msg = general_purpose::URL_SAFE
            .decode(value)
            .map_err(|e| anyhow!("Failed to decode b64 encoded message: {}", e))
            .and_then(|b64_decoded| {
                fvm_ipld_encoding::from_slice::<SignedMessage>(&b64_decoded)
                    .map_err(|e| anyhow!("Failed to deserialize signed message: {}", e))
            })?;
        self.signed_msg = Some(signed_msg);
        Ok(())
    }

    async fn read_object(&mut self, form_part: Part) -> anyhow::Result<()> {
        let mut temp_file = TempFile::new()
            .await
            .map_err(|e| anyhow!("failed to create temporary file: {}", e))?;
        let mut part_stream = form_part.stream();

        while let Some(data) = part_stream.next().await {
            let mut data = data?;
            while data.remaining() > 0 {
                let chunk = data.chunk().to_owned();
                let chunk_len = chunk.len();
                temp_file.write_all(&chunk).await?;
                temp_file.flush().await?;
                data.advance(chunk_len);
            }
        }
        temp_file
            .rewind()
            .await
            .map_err(|e| anyhow!("failed to rewind temporary file: {}", e))?;

        self.temp_file = Some(temp_file);
        Ok(())
    }

    async fn read_form(mut form_parts: warp::multipart::FormData) -> anyhow::Result<Self> {
        let mut object_parser = ObjectParser::default();
        while let Some(part) = form_parts.next().await {
            let part = part.map_err(|_| anyhow!("cannot read form data"))?;
            match part.name() {
                "chain_id" => {
                    object_parser.read_chain_id(part).await?;
                }
                "msg" => {
                    object_parser.read_msg(part).await?;
                }
                "object" => {
                    object_parser.read_object(part).await?;
                }
                _ => {
                    return Err(anyhow!("unknown form field"));
                }
            }
        }
        Ok(object_parser)
    }
}

async fn ensure_balance<F: QueryClient>(client: &F, from: Address) -> anyhow::Result<()> {
    let actor_state = client.actor_state(&from, FvmQueryHeight::Committed).await?;
    let balance = match actor_state.value {
        Some((_, state)) => to_eth_tokens(&state.balance)?,
        None => et::U256::zero(),
    };

    // TODO: make cost_per_byte a configurable constant
    // TODO: uncomment it when we decide the pricing logic
    // let cost_per_byte = et::U256::from(1_000_000_000u128);
    // let required_balance = cost_per_byte * self.size;
    if balance <= et::U256::zero() {
        return Err(anyhow!("insufficient balance"));
    }

    Ok(())
}

async fn health() -> Result<impl Reply, Rejection> {
    Ok(warp::reply::reply())
}

async fn handle_object_upload<F: QueryClient, I: IpfsApiAdapter>(
    client: F,
    ipfs: I,
    form_parts: warp::multipart::FormData,
) -> Result<impl Reply, Rejection> {
    let parser = ObjectParser::read_form(form_parts).await.map_err(|e| {
        Rejection::from(BadRequest {
            message: format!("failed to read form: {}", e),
        })
    })?;

    // Verify the signature
    let signed_msg = match parser.signed_msg {
        Some(signed_msg) => signed_msg,
        None => {
            return Err(Rejection::from(BadRequest {
                message: "missing signed message".to_string(),
            }))
        }
    };
    signed_msg.verify(&parser.chain_id).map_err(|e| {
        Rejection::from(BadRequest {
            message: e.to_string(),
        })
    })?;

    // Ensure the sender has enough balance, and add the data to IPFS
    let SignedMessage {
        object, message, ..
    } = signed_msg;
    ensure_balance(&client, message.from).await.map_err(|e| {
        Rejection::from(BadRequest {
            message: format!("failed to ensure balance: {}", e),
        })
    })?;
    let client_cid = match object {
        Some(object) => object.value,
        None => {
            return Err(Rejection::from(BadRequest {
                message: "missing CID in signed message".to_string(),
            }))
        }
    };
    let file = match parser.temp_file {
        Some(file) => file,
        None => {
            return Err(Rejection::from(BadRequest {
                message: "missing file in form".to_string(),
            }))
        }
    };
    let cid = ipfs.add_file(file, client_cid).await.map_err(|e| {
        Rejection::from(BadRequest {
            message: format!("failed to add file: {}", e),
        })
    })?;

    Ok(cid.to_string())
}

fn get_range_params(range: String, size: u64) -> Result<(u64, u64), ObjectsError> {
    let range: Vec<String> = range
        .replace("bytes=", "")
        .split('-')
        .map(|n| n.to_string())
        .collect();
    if range.len() != 2 {
        return Err(ObjectsError::RangeHeaderInvalid);
    }
    let (start, end): (u64, u64) = match (!range[0].is_empty(), !range[1].is_empty()) {
        (true, true) => (range[0].parse::<u64>()?, range[1].parse::<u64>()?),
        (true, false) => (range[0].parse::<u64>()?, size - 1),
        (false, true) => {
            let last = range[1].parse::<u64>()?;
            if last > size {
                (0, size - 1)
            } else {
                (size - last, size - 1)
            }
        }
        (false, false) => (0, size - 1),
    };
    if start > end || end >= size {
        return Err(ObjectsError::RangeHeaderInvalid);
    }
    Ok((start, end))
}

// TODO: Deprecated, remove after SDK migration
#[allow(clippy::too_many_arguments)]
async fn handle_object_download_deprecated(
    address: Address,
    tail: Tail,
    height_query: HeightQuery,
    list_query: ListQuery,
    range: Option<String>,
    client: FendermintClient,
    ipfs: IpfsClient,
    args: TransArgs,
) -> Result<impl Reply, Rejection> {
    let path = tail.as_str();
    if path.is_empty() || path.ends_with('/') {
        return handle_os_list(address, path, height_query, list_query, client, args).await;
    }

    let key: Vec<u8> = path.into();
    let height = height_query.height.unwrap_or(0);

    let res = os_get(client, args, address, GetParams { key }, height)
        .await
        .map_err(|e| {
            Rejection::from(BadRequest {
                message: format!("objectstore get error: {}", e),
            })
        })?;

    match res {
        Some(obj) => {
            let (body, start, end, len, size) = match obj {
                Object::Internal(buf) => {
                    let size = buf.0.len() as u64;
                    match range {
                        Some(range) => {
                            let (start, end) = get_range_params(range, size).map_err(|e| {
                                Rejection::from(BadRequest {
                                    message: format!("failed to get range params: {}", e),
                                })
                            })?;
                            let len = end - start + 1;
                            (
                                warp::hyper::Body::from(
                                    buf.0[start as usize..=end as usize].to_vec(),
                                ),
                                start,
                                end,
                                len,
                                size,
                            )
                        }
                        None => (warp::hyper::Body::from(buf.0), 0, size - 1, size, size),
                    }
                }
                Object::External((buf, resolved)) => {
                    let cid = Cid::try_from(buf.0).map_err(|e| {
                        Rejection::from(BadRequest {
                            message: format!("failed to decode cid: {}", e),
                        })
                    })?;
                    let cid = cid.to_string();
                    if !resolved {
                        return Err(Rejection::from(BadRequest {
                            message: "object is not resolved".to_string(),
                        }));
                    }

                    let stat = ipfs
                        .files_stat(format!("/ipfs/{cid}").as_str())
                        .await
                        .map_err(|e| {
                            Rejection::from(BadRequest {
                                message: format!("failed to stat object: {}", e),
                            })
                        })?;
                    let size = stat.size;

                    match range {
                        Some(range) => {
                            let (start, end) = get_range_params(range, size).map_err(|e| {
                                Rejection::from(BadRequest {
                                    message: format!("failed to get range params: {}", e),
                                })
                            })?;
                            let len = end - start + 1;
                            (
                                warp::hyper::Body::wrap_stream(ipfs.cat_range(
                                    &cid,
                                    start as usize,
                                    len as usize,
                                )),
                                start,
                                end,
                                len,
                                size,
                            )
                        }
                        None => (
                            warp::hyper::Body::wrap_stream(ipfs.cat(&cid)),
                            0,
                            size - 1,
                            size,
                            size,
                        ),
                    }
                }
            };

            let mut response = warp::reply::Response::new(body);
            let mut header_map = HeaderMap::new();
            if len < size {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                header_map.insert(
                    "Content-Range",
                    HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, len)).unwrap(),
                );
            } else {
                header_map.insert("Accept-Ranges", HeaderValue::from_str("bytes").unwrap());
            }
            header_map.insert("Content-Length", HeaderValue::from(len));
            let headers = response.headers_mut();
            headers.extend(header_map);

            Ok(response)
        }
        None => Err(Rejection::from(NotFound)),
    }
}

// TODO: Deprecated, remove after SDK migration
async fn handle_os_list(
    address: Address,
    mut prefix: &str,
    height_query: HeightQuery,
    list_query: ListQuery,
    client: FendermintClient,
    args: TransArgs,
) -> Result<warp::reply::Response, Rejection> {
    if prefix == "/" {
        prefix = "";
    }
    let params = ListParams {
        prefix: prefix.into(),
        delimiter: "/".into(),
        offset: list_query.offset.unwrap_or(0),
        limit: list_query.limit.unwrap_or(0),
    };
    let height = height_query.height.unwrap_or(0);

    let res = os_list(client, args, address, params, height)
        .await
        .map_err(|e| {
            Rejection::from(BadRequest {
                message: format!("objectstore list error: {}", e),
            })
        })?;

    let list = res.unwrap_or_default();
    let objects = list
        .objects
        .iter()
        .map(|v| {
            let key = core::str::from_utf8(&v.0).unwrap_or_default().to_string();
            match &v.1 {
                ObjectListItem::Internal((cid, size)) => {
                    json!({"key": key, "value": json!({"kind": "internal", "content": cid.to_string(), "size": size})})
                }
                ObjectListItem::External((cid, resolved)) => {
                    json!({"key": key, "value": json!({"kind": "external", "content": cid.to_string(), "resolved": resolved})})
                }
            }
        })
        .collect::<Vec<Value>>();
    let common_prefixes = list
        .common_prefixes
        .iter()
        .map(|v| Value::String(core::str::from_utf8(v).unwrap_or_default().to_string()))
        .collect::<Vec<Value>>();

    let list = json!({"objects": objects, "common_prefixes": common_prefixes});
    let list = serde_json::to_vec(&list).unwrap();
    let mut header_map = HeaderMap::new();
    header_map.insert("Content-Length", HeaderValue::from(list.len()));
    header_map.insert("Content-Type", HeaderValue::from_static("application/json"));
    let body = warp::hyper::Body::from(list);
    let mut response = warp::reply::Response::new(body);
    let headers = response.headers_mut();
    headers.extend(header_map);

    Ok(response)
}

struct ObjectRange {
    start: u64,
    end: u64,
    len: u64,
    size: u64,
    body: warp::hyper::Body,
}

#[allow(clippy::too_many_arguments)]
async fn handle_object_download(
    address: Address,
    key: String,
    method: String,
    range: Option<String>,
    height_query: HeightQuery,
    client: FendermintClient,
    ipfs: IpfsClient,
    args: TransArgs,
) -> Result<impl Reply, Rejection> {
    let height = height_query
        .height
        .unwrap_or(FvmQueryHeight::Committed.into());
    let maybe_object = os_get(client, args, address, GetParams { key: key.into() }, height)
        .await
        .map_err(|e| {
            Rejection::from(BadRequest {
                message: format!("objectstore get error: {}", e),
            })
        })?;

    match maybe_object {
        Some(object) => {
            let object_range = match object {
                Object::Internal(_) => {
                    return Err(Rejection::from(BadRequest {
                        message: "internal objects are not supported".to_string(),
                    }))
                }
                Object::External((buf, resolved)) => {
                    let cid = Cid::try_from(buf.0).map_err(|e| {
                        Rejection::from(BadRequest {
                            message: format!("failed to decode cid: {}", e),
                        })
                    })?;
                    if !resolved {
                        return Err(Rejection::from(BadRequest {
                            message: "object is not resolved".to_string(),
                        }));
                    }
                    fetch_object(ipfs, range, cid.into()).await.map_err(|e| {
                        Rejection::from(BadRequest {
                            message: format!("failed to fetch detached object {}", e),
                        })
                    })?
                }
            };

            // If it is a HEAD request, we don't need to send the body
            // but we still need to send the Content-Length header
            if method == "HEAD" {
                let mut response = warp::reply::Response::new(warp::hyper::Body::empty());
                let mut header_map = HeaderMap::new();
                header_map.insert("Content-Length", HeaderValue::from(object_range.size));
                let headers = response.headers_mut();
                headers.extend(header_map);
                return Ok(response);
            }

            let mut response = warp::reply::Response::new(object_range.body);
            let mut header_map = HeaderMap::new();
            if object_range.len < object_range.size {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                header_map.insert(
                    "Content-Range",
                    HeaderValue::from_str(&format!(
                        "bytes {}-{}/{}",
                        object_range.start, object_range.end, object_range.len
                    ))
                    .unwrap(),
                );
            } else {
                header_map.insert("Accept-Ranges", HeaderValue::from_str("bytes").unwrap());
            }
            header_map.insert("Content-Length", HeaderValue::from(object_range.len));
            let headers = response.headers_mut();
            headers.extend(header_map);

            Ok(response)
        }
        None => Err(Rejection::from(NotFound)),
    }
}

async fn fetch_object(
    ipfs: IpfsClient,
    range: Option<String>,
    cid: String,
) -> anyhow::Result<ObjectRange> {
    let stat = ipfs.files_stat(format!("/ipfs/{cid}").as_str()).await?;
    let size = stat.size;
    Ok(match range {
        Some(range) => {
            let (start, end) = get_range_params(range, size)?;
            let len = end - start + 1;
            let body = Body::wrap_stream(ipfs.cat_range(&cid, start as usize, len as usize));
            ObjectRange {
                start,
                end,
                len,
                size,
                body,
            }
        }
        None => ObjectRange {
            start: 0,
            end: size - 1,
            len: size,
            size,
            body: Body::wrap_stream(ipfs.cat(&cid)),
        },
    })
}

// Rejection handlers

#[derive(Clone, Debug)]
struct BadRequest {
    message: String,
}

impl warp::reject::Reject for BadRequest {}

#[derive(Debug)]
struct NotFound;

impl warp::reject::Reject for NotFound {}

#[derive(Clone, Debug, Serialize)]
struct ErrorMessage {
    code: u16,
    message: String,
}

async fn handle_rejection(err: Rejection) -> Result<impl Reply, Infallible> {
    let (code, message) = if err.is_not_found() || err.find::<NotFound>().is_some() {
        (StatusCode::NOT_FOUND, "Not Found".to_string())
    } else if let Some(e) = err.find::<BadRequest>() {
        let err = e.to_owned();
        (StatusCode::BAD_REQUEST, err.message)
    } else if err.find::<warp::reject::PayloadTooLarge>().is_some() {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Payload too large".to_string(),
        )
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:?}", err))
    };

    let reply = warp::reply::json(&ErrorMessage {
        code: code.as_u16(),
        message,
    });
    let reply = warp::reply::with_header(reply, "Access-Control-Allow-Origin", "*");
    Ok(warp::reply::with_status(reply, code))
}

// RPC methods

async fn os_get(
    client: FendermintClient,
    args: TransArgs,
    address: Address,
    params: GetParams,
    height: u64,
) -> anyhow::Result<Option<Object>> {
    let mut client = TransClient::new(client, &args)?;
    let gas_params = gas_params(&args);
    let h = FvmQueryHeight::from(height);

    let res = client
        .inner
        .os_get_call(address, params, TokenAmount::default(), gas_params, h)
        .await?;

    Ok(res.return_data)
}

// TODO: Deprecated, remove after SDK migration
async fn os_list(
    client: FendermintClient,
    args: TransArgs,
    address: Address,
    params: ListParams,
    height: u64,
) -> anyhow::Result<Option<ObjectList>> {
    let mut client = TransClient::new(client, &args)?;
    let gas_params = gas_params(&args);
    let h = FvmQueryHeight::from(height);

    let res = client
        .inner
        .os_list_call(address, params, TokenAmount::default(), gas_params, h)
        .await?;

    Ok(res.return_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cid::multihash::{Code, MultihashDigest};
    use ethers::core::k256::ecdsa::SigningKey;
    use ethers::core::rand::{rngs::StdRng, SeedableRng};
    use fendermint_actor_objectstore::{ObjectKind, PutParams};
    use fendermint_rpc::FendermintClient;
    use fendermint_vm_message::conv::from_eth::to_fvm_address;
    use fvm_ipld_encoding::RawBytes;
    use tendermint_rpc::{Method, MockClient, MockRequestMethodMatcher};

    pub struct IpfsMocked {
        _inner: IpfsClient,
    }

    impl IpfsApiAdapter for IpfsMocked {
        async fn add_file(&self, _temp_file: TempFile, _cid: Cid) -> anyhow::Result<String> {
            Ok("Qm123".to_string())
        }
    }

    // Used to mocking Actor State
    const ABCI_QUERY_RESPONSE: &str = r#"{
        "jsonrpc": "2.0",
        "id": "",
        "result": {
         "response": {
             "code": 0,
             "log": "",
             "info": "",
             "index": "0",
             "key": "GGQ=",
             "value": "pWRjb2Rl2CpYJwABVaDkAiB4ZQKaqaSEiu8tIb2Ef7bIWOoxPeNkAEljZabMaAMlaGVzdGF0ZdgqWCcAAXGg5AIgRbDPwiDO7Ft8HGLE1Bk9OOTrpI6IFXKc51+cCrDkwcBoc2VxdWVuY2UAZ2JhbGFuY2VKADY1ya3F3qAAAHFkZWxlZ2F0ZWRfYWRkcmVzc1YECqf8cO8ArRpoWzmcqFtkasWDXCqx",
             "proof": null,
             "height": "8",
             "codespace": ""
           }
        }
     }"#;

    fn form_body(
        boundary: &str,
        serialized_signed_message_b64: &str,
        external_object: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "\
            --{0}\r\n\
            content-disposition: form-data; name=\"chain_id\"\r\n\r\n\
            314159\r\n\
            --{0}\r\n\
            content-disposition: form-data; name=\"msg\"\r\n\r\n\
            {1}\r\n\
            --{0}\r\n\
            ",
                boundary, serialized_signed_message_b64
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            "Content-Disposition: form-data; name=\"object\"; filename=\"example.bin\"\r\n\
                Content-Type: application/octet-stream\r\n\r\n"
                .to_string()
                .as_bytes(),
        );
        body.extend_from_slice(external_object);
        body.extend_from_slice(format!("\r\n--{0}--\r\n", boundary).as_bytes());
        body
    }

    async fn multipart_form(
        serialized_signed_message_b64: &str,
        external_object: &[u8],
    ) -> warp::multipart::FormData {
        let boundary = "--abcdef1234--";
        let body = form_body(boundary, serialized_signed_message_b64, external_object);
        warp::test::request()
            .method("POST")
            .header("content-length", body.len())
            .header(
                "content-type",
                format!("multipart/form-data; boundary={}", boundary),
            )
            .body(body)
            .filter(&warp::multipart::form())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_handle_object_upload() {
        let matcher = MockRequestMethodMatcher::default()
            .map(Method::AbciQuery, Ok(ABCI_QUERY_RESPONSE.to_string()));
        let client = FendermintClient::new(MockClient::new(matcher).0);
        let ipfs = IpfsMocked {
            _inner: IpfsClient::default(),
        };

        let key = b"key";
        let external_object = b"hello world".as_ref();
        let digest = Code::Blake2b256.digest(external_object);
        let object_cid = Cid::new_v1(fvm_ipld_encoding::IPLD_RAW, digest);
        let params = PutParams {
            key: key.to_vec(),
            kind: ObjectKind::External(object_cid),
            overwrite: true,
        };
        let params = RawBytes::serialize(params).unwrap();
        let to = Address::new_id(90);
        let object = fendermint_vm_message::signed::Object::new(key.to_vec(), object_cid, to);

        let sk = fendermint_crypto::SecretKey::random(&mut StdRng::from_entropy());
        let signing_key = SigningKey::from_slice(sk.serialize().as_ref()).unwrap();
        let from_address = ethers::core::utils::secret_key_to_address(&signing_key);
        let message = fvm_shared::message::Message {
            version: Default::default(),
            from: to_fvm_address(from_address),
            to,
            sequence: 0,
            value: TokenAmount::from_atto(0),
            method_num: fendermint_actor_objectstore::Method::PutObject as u64,
            params,
            gas_limit: 3000000,
            gas_fee_cap: TokenAmount::from_atto(0),
            gas_premium: TokenAmount::from_atto(0),
        };
        let chain_id = fvm_shared::chainid::ChainID::from(314159);
        let signed = fendermint_vm_message::signed::SignedMessage::new_secp256k1(
            message,
            Some(object),
            &sk,
            &chain_id,
        )
        .unwrap();

        let serialized_signed_message = fvm_ipld_encoding::to_vec(&signed).unwrap();
        let serialized_signed_message_b64 =
            general_purpose::URL_SAFE.encode(&serialized_signed_message);

        let multipart_form = multipart_form(&serialized_signed_message_b64, external_object).await;
        let reply = handle_object_upload(client, ipfs, multipart_form)
            .await
            .unwrap();
        let response = reply.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
