use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

use anyhow::Result;
use deno_core::{
    error::AnyError, op2, AsyncRefCell, Extension, FeatureChecker, FsModuleLoader, ModuleSpecifier,
    OpState, RcRef, Resource, ResourceId,
};
use deno_runtime::{
    deno_broadcast_channel::InMemoryBroadcastChannel,
    deno_fs::{FileSystem, RealFs},
    deno_io::Stdio,
    deno_web::BlobStore,
    ops::worker_host::CreateWebWorkerCb,
    permissions::PermissionsContainer,
    web_worker::{WebWorker, WebWorkerOptions},
    worker::{MainWorker, WorkerOptions},
    BootstrapOptions,
};
use iroh_bytes::Hash;
use serde::{Deserialize, Serialize};

use futures::{stream::BoxStream, StreamExt};
use iroh::{
    client::mem::{Doc, Iroh},
    rpc_protocol::NodeStatusResponse,
    sync_engine::LiveEvent,
};
use iroh_sync::NamespaceId;

pub(crate) async fn exec(iroh: &Iroh, js_path: PathBuf) -> Result<()> {
    let js_path = js_path.canonicalize()?;
    println!("Loading {}", js_path.display());

    let main_module =
        ModuleSpecifier::from_file_path(js_path).map_err(|_| anyhow::anyhow!("invalid js path"))?;
    let shared = Shared {
        fs: Arc::new(RealFs),
        blob_store: Default::default(),
        broadcast_channel: Default::default(),
        stdio: Default::default(),
        feature_checker: Default::default(),
        location: Some(main_module.clone()),
        iroh: iroh.clone(),
    };

    let create_web_worker_cb = create_web_worker_callback(shared.clone());
    let extensions = shared.extensions();

    let mut worker = MainWorker::bootstrap_from_options(
        main_module.clone(),
        PermissionsContainer::allow_all(),
        WorkerOptions {
            fs: shared.fs,
            module_loader: Rc::new(FsModuleLoader),
            blob_store: shared.blob_store,
            broadcast_channel: shared.broadcast_channel,
            stdio: shared.stdio,
            feature_checker: shared.feature_checker,
            extensions,
            create_web_worker_cb,
            bootstrap: BootstrapOptions {
                location: Some(main_module.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    worker.execute_main_module(&main_module).await?;
    worker.run_event_loop(false).await?;

    Ok(())
}

#[derive(Clone)]
struct Shared {
    fs: Arc<dyn FileSystem>,
    blob_store: Arc<BlobStore>,
    broadcast_channel: InMemoryBroadcastChannel,
    stdio: Stdio,
    feature_checker: Arc<FeatureChecker>,
    location: Option<ModuleSpecifier>,
    iroh: Iroh,
}

impl Shared {
    fn extensions(&self) -> Vec<Extension> {
        vec![iroh_runtime::init_ops_and_esm(self.iroh.clone())]
    }
}

fn create_web_worker_callback(shared: Shared) -> Arc<CreateWebWorkerCb> {
    Arc::new(move |args| {
        let create_web_worker_cb = create_web_worker_callback(shared.clone());
        let options = WebWorkerOptions {
            bootstrap: BootstrapOptions {
                location: shared.location.clone(),
                ..Default::default()
            },
            extensions: shared.extensions(),
            startup_snapshot: None,
            unsafely_ignore_certificate_errors: None,
            root_cert_store_provider: None,
            seed: None,
            fs: shared.fs.clone(),
            module_loader: Rc::new(FsModuleLoader),
            npm_resolver: None,
            create_web_worker_cb,
            format_js_error_fn: None,
            source_map_getter: None,
            worker_type: args.worker_type,
            maybe_inspector_server: None,
            get_error_class_fn: None,
            blob_store: shared.blob_store.clone(),
            broadcast_channel: shared.broadcast_channel.clone(),
            compiled_wasm_module_store: None,
            shared_array_buffer_store: None,
            cache_storage_dir: None,
            stdio: shared.stdio.clone(),
            feature_checker: shared.feature_checker.clone(),
        };

        WebWorker::bootstrap_from_options(
            args.name,
            args.permissions,
            args.main_module,
            args.worker_id,
            options,
        )
    })
}

deno_core::extension!(
    iroh_runtime,
    ops = [op_node_status, op_doc_subscribe, op_next_doc_event, op_doc_create, op_doc_set, op_blob_get],
    esm_entry_point = "ext:iroh_runtime/bootstrap.js",
    esm = [dir "src", "bootstrap.js"],
    options = { iroh: Iroh },
    state = move |state, options| {
        state.put::<Iroh>(options.iroh);
    },
);

#[op2(async)]
#[serde]
async fn op_node_status(state: Rc<RefCell<OpState>>) -> Result<NodeStatusResponse, AnyError> {
    let iroh = {
        let state = state.borrow();
        state.borrow::<Iroh>().clone()
    };

    let status = iroh.node.status().await?;
    Ok(status)
}

#[op2(async)]
#[serde]
async fn op_next_doc_event(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
) -> Result<Option<LiveEvent>, AnyError> {
    let sub = state.borrow_mut().resource_table.get::<DocSub>(rid)?;
    let mut stream = RcRef::map(&sub, |s| &s.sub).borrow_mut().await;
    let event = stream.next().await.transpose()?;
    Ok(event)
}

#[op2(async)]
#[smi]
async fn op_doc_subscribe(
    state: Rc<RefCell<OpState>>,
    #[serde] doc_js_wrapper: DocJsWrapper,
) -> Result<ResourceId, AnyError> {
    println!("subscribing");
    let sub = if state.borrow().resource_table.has(doc_js_wrapper.rid) {
        let doc_wrapper = state
            .borrow_mut()
            .resource_table
            .get::<DocWrapper>(doc_js_wrapper.rid)?;
        println!("fast path");
        // fast path, same thread
        let doc = RcRef::map(&doc_wrapper, |d| &d.0);
        doc.subscribe().await?
    } else {
        println!("slow path: fetching iroh");
        let iroh = {
            let state = state.borrow();
            state.borrow::<Iroh>().clone()
        };

        // slow path, different worker thread
        let doc = iroh
            .docs
            .open(doc_js_wrapper.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown doc"))?;
        doc.subscribe().await?
    };
    let sub = DocSub {
        sub: AsyncRefCell::new(sub.boxed()),
    };
    println!("created sub");

    let rid = state.borrow_mut().resource_table.add(sub);
    println!("stored sub");
    Ok(rid)
}

#[op2(async)]
#[string]
async fn op_doc_set(
    state: Rc<RefCell<OpState>>,
    #[serde] doc_js_wrapper: DocJsWrapper,
    #[string] key: String,
    #[string] value: String,
) -> Result<String, AnyError> {
    let iroh = {
        let state = state.borrow();
        state.borrow::<Iroh>().clone()
    };

    // TODO: pass author
    let author = iroh.authors.create().await?;

    let hash = if state.borrow().resource_table.has(doc_js_wrapper.rid) {
        // fast path, same thread
        let doc_wrapper = state
            .borrow_mut()
            .resource_table
            .get::<DocWrapper>(doc_js_wrapper.rid)?;
        let doc = RcRef::map(&doc_wrapper, |d| &d.0);
        doc.set_bytes(author, key, value).await?
    } else {
        // slow path, different worker thread
        let doc = iroh
            .docs
            .open(doc_js_wrapper.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown doc"))?;
        doc.set_bytes(author, key, value).await?
    };

    Ok(hash.to_string())
}

#[op2(async)]
#[string]
async fn op_blob_get(
    state: Rc<RefCell<OpState>>,
    #[string] hash: String,
) -> Result<String, AnyError> {
    let hash: Hash = hash.parse()?;

    let iroh = {
        let state = state.borrow();
        state.borrow::<Iroh>().clone()
    };

    let res = iroh.blobs.read_to_bytes(hash).await?;
    let res = std::str::from_utf8(&res)?.to_owned();

    Ok(res)
}

#[op2(async)]
#[serde]
async fn op_doc_create(state: Rc<RefCell<OpState>>) -> Result<DocJsWrapper, AnyError> {
    let iroh = {
        let state = state.borrow();
        state.borrow::<Iroh>().clone()
    };

    let doc = iroh.docs.create().await?;
    let id = doc.id();
    let rid = state.borrow_mut().resource_table.add(DocWrapper(doc));

    Ok(DocJsWrapper { rid, id })
}

#[derive(Serialize, Deserialize, Clone)]
struct DocJsWrapper {
    rid: ResourceId,
    id: NamespaceId,
}

struct DocWrapper(Doc);
impl Resource for DocWrapper {}

struct DocSub {
    sub: AsyncRefCell<BoxStream<'static, anyhow::Result<LiveEvent>>>,
}

impl Resource for DocSub {}