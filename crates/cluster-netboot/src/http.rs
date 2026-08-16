use crate::cloudinit::CloudInitGenerator;
use axum::{
    extract::{ConnectInfo, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use cluster_common::crd::{NodeRole, PiNode, PiNodeSpec};
use kube::api::ListParams;
use kube::{Api, Client};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct HttpState {
    pub assets_dir: PathBuf,
    pub vip: String,
    pub http_port: u16,
    pub k3s_token: String,
    pub kube_client: Option<Client>,
    pub namespace: String,
}

#[derive(Deserialize)]
pub struct NodeQuery {
    pub mac: Option<String>,
    pub serial: Option<String>,
}

pub fn create_router(state: Arc<HttpState>) -> Router {
    let assets_service = ServeDir::new(&state.assets_dir);

    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/cmdline.txt", get(cmdline_handler))
        .route("/cloud-init/user-data", get(userdata_handler))
        .route("/cloud-init/meta-data", get(metadata_handler))
        .nest_service("/assets", assets_service)
        .with_state(state)
}

async fn healthz_handler() -> &'static str {
    "ok"
}

async fn cmdline_handler(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    let cmdline = CloudInitGenerator::generate_cmdline(&state.vip, state.http_port);
    (StatusCode::OK, [("content-type", "text/plain")], cmdline)
}

async fn userdata_handler(
    State(state): State<Arc<HttpState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<NodeQuery>,
) -> Response {
    let node = resolve_node(&state, &peer, &query).await;
    let userdata = CloudInitGenerator::generate_user_data(&node, &state.vip, &state.k3s_token);
    (StatusCode::OK, [("content-type", "text/yaml")], userdata).into_response()
}

async fn metadata_handler(
    State(state): State<Arc<HttpState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<NodeQuery>,
) -> Response {
    let node = resolve_node(&state, &peer, &query).await;
    let metadata = CloudInitGenerator::generate_meta_data(&node);
    (StatusCode::OK, [("content-type", "text/yaml")], metadata).into_response()
}

async fn resolve_node(state: &HttpState, peer: &SocketAddr, query: &NodeQuery) -> PiNode {
    if let Some(ref client) = state.kube_client {
        let api: Api<PiNode> = Api::namespaced(client.clone(), &state.namespace);
        if let Ok(list) = api.list(&ListParams::default()).await {
            for node in list.items {
                if let Some(ref mac) = query.mac {
                    if node.spec.mac_address.eq_ignore_ascii_case(mac) {
                        return node;
                    }
                }
                if let Some(ref serial) = query.serial {
                    if &node.spec.hardware_serial == serial {
                        return node;
                    }
                }
                if let Some(ref ip) = node.spec.ip_address {
                    if ip == &peer.ip().to_string() {
                        return node;
                    }
                }
            }
        }
    }

    // Default fallback node if not yet registered in k8s
    let fallback_serial = query
        .serial
        .clone()
        .unwrap_or_else(|| format!("unknown-{}", peer.ip()));

    PiNode {
        metadata: kube::core::ObjectMeta {
            name: Some(format!("node-{}", fallback_serial)),
            ..Default::default()
        },
        spec: PiNodeSpec {
            hardware_serial: fallback_serial,
            mac_address: query.mac.clone().unwrap_or_default(),
            desired_role: NodeRole::Worker,
            target_disk_id: None,
            reformat_confirmed: false,
            ip_address: Some(peer.ip().to_string()),
            hostname: None,
        },
        status: None,
    }
}
