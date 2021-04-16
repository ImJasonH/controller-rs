use crate::{telemetry, Error, Result};
use chrono::prelude::*;
use futures::{future::BoxFuture, FutureExt, StreamExt};
use kube::{
    api::{Api, ListParams, Patch, PatchParams, Resource},
    client::Client,
    CustomResource,
};
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use maplit::hashmap;
use prometheus::{
    default_registry, proto::MetricFamily, register_histogram_vec, register_int_counter, Exemplar,
    HistogramVec, IntCounter,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::{
    sync::RwLock,
    time::{Duration, Instant},
};
use tracing::{field, info, instrument, warn, Span};

///////// TYPES

/// Our Foo custom resource spec
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "imjasonh.com",
    version = "v1alpha1",
    kind = "Foo",
    namespaced,
    status = "FooStatus",
    printcolumn = r#"{"name":"Good", "type":"string", "description":"whether it's good", "jsonPath":".status.conditions[?(@.type==\"Good\")].status"}"#,
    printcolumn = r#"{"name":"Reason", "type":"string", "description":"reason it's good or not", "jsonPath":".status.conditions[?(@.type==\"Good\")].reason"}"#
)]
pub struct FooSpec {
    name: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct FooStatus {
    conditions: Vec<Condition>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
enum Type {
    Good,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
enum Status {
    True,
    False,
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
enum Severity {
    #[serde(rename = "")]
    Error,
    Warning,
    Info,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    type_: Type,
    status: Status,
    reason: String,
    message: String,
    severity: Severity,
    // TODO: patching this volatile time results in infinite reconciling :(
    // last_transition_time: DateTime<Utc>,
}

///////// CONTROLLER

// Context for our reconciler
#[derive(Clone)]
struct Data {
    /// kubernetes client
    client: Client,
    /// In memory state
    state: Arc<RwLock<State>>,
    /// Various prometheus metrics
    metrics: Metrics,
}

const RESYNC: Duration = Duration::from_secs(30 * 60);

#[instrument(skip(ctx), fields(trace_id))]
async fn reconcile(foo: Foo, ctx: Context<Data>) -> Result<ReconcilerAction, Error> {
    let trace_id = telemetry::get_trace_id();
    Span::current().record("traceId", &field::display(&trace_id));
    let start = Instant::now();

    let client = ctx.get_ref().client.clone();
    ctx.get_ref().state.write().await.last_event = Utc::now();
    let name = Resource::name(&foo);
    let ns = Resource::namespace(&foo).expect("foo is namespaced");
    let foos: Api<Foo> = Api::namespaced(client, &ns);

    let mut new_status = Patch::Apply(json!({
        "apiVersion": "imjasonh.com/v1alpha1",
        "kind": "Foo",
        "status": FooStatus {
            conditions: vec![Condition {
                type_: Type::Good,
                status: Status::True,
                severity: Severity::Info,
                reason: "Good".to_string(),
                message: "Name is good".to_string(),
                // last_transition_time: Utc::now(),
            }],
        },
    }));
    if name.contains("bad") {
        new_status = Patch::Apply(json!({
            "apiVersion": "imjasonh.com/v1alpha1",
            "kind": "Foo",
            "status": FooStatus {
                conditions: vec![Condition {
                    type_: Type::Good,
                    status: Status::False,
                    severity: Severity::Error,
                    reason: "NotGood".to_string(),
                    message: "Name is bad".to_string(),
                    // last_transition_time: Utc::now(),
                }],
            },
        }));
    }
    let ps = PatchParams::apply("cntrlr").force();
    let _o = foos
        .patch_status(&name, &ps, &new_status)
        .await
        .map_err(Error::KubeError)?;

    let duration = start.elapsed().as_millis() as f64 / 1000.0;
    let ex = Exemplar::new_with_labels(duration, hashmap! {"trace_id".to_string() => trace_id});
    ctx.get_ref()
        .metrics
        .reconcile_duration
        .with_label_values(&[])
        .observe_with_exemplar(duration, ex);
    ctx.get_ref().metrics.handled_events.inc();
    info!(
        "Reconciled Foo \"{}\" in namespace \"{}\" in {}",
        name, ns, duration
    );

    // If no events were received, check back every 30 minutes
    Ok(ReconcilerAction {
        requeue_after: Some(RESYNC),
    })
}

fn error_policy(error: &Error, _ctx: Context<Data>) -> ReconcilerAction {
    warn!("reconcile failed: {:?}", error);
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(360)),
    }
}

///////// METRICS

/// Metrics exposed on /metrics
#[derive(Clone)]
pub struct Metrics {
    pub handled_events: IntCounter,
    pub reconcile_duration: HistogramVec,
}
impl Metrics {
    fn new() -> Self {
        let reconcile_histogram = register_histogram_vec!(
            "foo_controller_reconcile_duration_seconds",
            "The duration of reconcile to complete in seconds",
            &[],
            vec![0.01, 0.1, 0.25, 0.5, 1., 5., 15., 60.]
        )
        .unwrap();

        Metrics {
            handled_events: register_int_counter!("foo_controller_handled_events", "handled events").unwrap(),
            reconcile_duration: reconcile_histogram,
        }
    }
}

/// In-memory reconciler state exposed on /
#[derive(Clone, Serialize)]
pub struct State {
    #[serde(deserialize_with = "from_ts")]
    pub last_event: DateTime<Utc>,
}
impl State {
    fn new() -> Self {
        State {
            last_event: Utc::now(),
        }
    }
}

///////// ENTRYPOINT

/// Data owned by the Manager
#[derive(Clone)]
pub struct Manager {
    /// In memory state
    state: Arc<RwLock<State>>,
    /// Various prometheus metrics
    metrics: Metrics,
}

/// Example Manager that owns a Controller for Foo
impl Manager {
    /// Lifecycle initialization interface for app
    ///
    /// This returns a `Manager` that drives a `Controller` + a future to be awaited
    /// It is up to `main` to wait for the controller stream.
    pub async fn new() -> (Self, BoxFuture<'static, ()>) {
        let client = Client::try_default().await.expect("create client");
        let metrics = Metrics::new();
        let state = Arc::new(RwLock::new(State::new()));
        let context = Context::new(Data {
            client: client.clone(),
            metrics: metrics.clone(),
            state: state.clone(),
        });

        let foos = Api::<Foo>::all(client);
        // Ensure CRD is installed before loop-watching
        let _r = foos
            .list(&ListParams::default().limit(1))
            .await
            .expect("is the crd installed? please run: cargo run --bin crdgen | kubectl apply -f -");

        // All good. Start controller and return its future.
        let drainer = Controller::new(foos, ListParams::default())
            .run(reconcile, error_policy, context)
            .filter_map(|x| async move { std::result::Result::ok(x) })
            .for_each(|_| futures::future::ready(()))
            .boxed();

        (Self { state, metrics }, drainer)
    }

    /// Metrics getter
    pub fn metrics(&self) -> Vec<MetricFamily> {
        default_registry().gather()
    }

    /// State getter
    pub async fn state(&self) -> State {
        self.state.read().await.clone()
    }
}
