use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use anyhow::anyhow;
use wasmtime::{Config, Engine, InstanceAllocationStrategy, LinearMemory, MemoryCreator, MemoryType, ResourceLimiterAsync, Result, Store};
use wasmtime::component::*;
use wasmtime_runtime::{DefaultMemoryCreator, RuntimeMemoryCreator};
use wasmtime_wasi::preview2::{Table, WasiCtx, WasiCtxBuilder, WasiView};


fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
}

struct Ctx {
    table: Table,
    wasi: WasiCtx,
}

impl WasiView for Ctx {
    fn table(&self) -> &Table {
        &self.table
    }
    fn table_mut(&mut self) -> &mut Table {
        &mut self.table
    }
    fn ctx(&self) -> &WasiCtx {
        &self.wasi
    }
    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

struct TestMemoryCreator;

struct TestLinearMemory;

unsafe impl MemoryCreator for TestMemoryCreator {
    fn new_memory(&self, ty: MemoryType, minimum: usize, maximum: Option<usize>, reserved_size_in_bytes: Option<usize>, guard_size_in_bytes: usize) -> Result<Box<dyn LinearMemory>, String> {
        println!("new_memory: ty={:?}, minimum={}, maximum={:?}, reserved_size_in_bytes={:?}, guard_size_in_bytes={}", ty, minimum, maximum, reserved_size_in_bytes, guard_size_in_bytes);
        Ok(Box::new(TestLinearMemory))
    }
}

unsafe impl LinearMemory for TestLinearMemory {
    fn byte_size(&self) -> usize {
        100
    }

    fn maximum_byte_size(&self) -> Option<usize> {
        None
    }

    fn grow_to(&mut self, new_size: usize) -> Result<()> {
        Ok(())
    }

    fn as_ptr(&self) -> *mut u8 {
        0 as *const u8 as *mut u8
    }

    fn wasm_accessible(&self) -> Range<usize> {
        0..100
    }
}

#[async_trait::async_trait]
impl ResourceLimiterAsync for Ctx {
    async fn memory_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> Result<bool> {
        println!("MEMORY GROWING current={}, desired={}, maximum={:?}", current, desired, maximum);
        Ok(true)
    }

    async fn table_growing(&mut self, current: u32, desired: u32, maximum: Option<u32>) -> Result<bool> {
        println!("TABLE GROWING current={}, desired={}, maximum={:?}", current, desired, maximum);
        Ok(true)
    }
}

async fn run_first() -> Result<Snapshot> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);
    config.coredump_on_trap(true);
    // config.wmemcheck(true);
    config.debug_info(true);
    // config.with_host_memory(Arc::new(TestMemoryCreator));
    config.allocation_strategy(InstanceAllocationStrategy::OnDemand);
    // config.memory_init_cow(false);
    config.cranelift_debug_verifier(true);
    config.static_memory_forced(true);

    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);

    wasmtime_wasi::preview2::command::add_to_linker(&mut linker)?;

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdio();
    let mut table = Table::new();
    let wasi = wasi_builder.build(&mut table)?;

    let ctx = Ctx {
        table,
        wasi,
    };

    let mut store = Store::new(&engine, ctx);

    store.limiter_async(|ctx| ctx as &mut dyn ResourceLimiterAsync);

    let component = Component::from_file(&engine, Path::new("/home/vigoo/projects/zivergetech/golem/integration-tests/src/it/wasm/shopping-cart.wasm"))?;
    // let component = Component::from_file(&engine, Path::new("/home/vigoo/projects/zivergetech/golem/integration-tests/src/it/wasm/python-1.wasm"))?;

    let instance_pre = linker.instantiate_pre(&component)?;
    let instance = instance_pre.instantiate_async(&mut store).await?;

    let (initialize_cart, add_item, get_cart_contents) = {
        let mut exports = instance.exports(&mut store);
        let mut api = exports.instance("golem:it/api").ok_or(anyhow!("cannot find golem:it/api"))?;
        (api.func("initialize-cart").ok_or(anyhow!("cannot find initialize-cart"))?,
         api.func("add-item").ok_or(anyhow!("cannot find add-item"))?,
         api.func("get-cart-contents").ok_or(anyhow!("cannot find get-cart-contents"))?)
    };

    let input1 = vec![Val::String("test shopping cart".to_string().into_boxed_str())];
    let mut output1: Vec<Val> = vec![];
    initialize_cart.call_async(&mut store, &input1, &mut output1).await?;
    initialize_cart.post_return_async(&mut store).await?;

    let input2 = vec![Val::Record(Record::new(
        &add_item.params(&store)[0].unwrap_record(),
        vec![
            ("product-id", Val::String("product-1".to_string().into_boxed_str())),
            ("name", Val::String("Some product".to_string().into_boxed_str())),
            ("price", Val::Float32(1234.567)),
            ("quantity", Val::U32(100)),
        ], )?)];
    let mut output2: Vec<Val> = vec![];
    add_item.call_async(&mut store, &input2, &mut output2).await?;
    add_item.post_return_async(&mut store).await?;

    let input3 = vec![];
    let mut output3: Vec<Val> = vec![Val::Bool(true)];

    get_cart_contents.call_async(&mut store, &input3, &mut output3).await?;
    get_cart_contents.post_return_async(&mut store).await?;

    println!("Contents before snapshotting: {output3:?}");

    let snapshot = instance.snapshot(&mut store)?;
    for memory in &snapshot.memories {
        println!("Saved a memory of {} bytes", memory.len());
    }
    for instance in &snapshot.globals {
        println!("Saved {} globals for an instance", instance.len());
    }

    dump_table(&store.data().table);

    Ok(snapshot)
}

async fn run_second(snapshot: Snapshot) -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);
    config.coredump_on_trap(true);
    // config.wmemcheck(true);
    config.debug_info(true);
    config.allocation_strategy(InstanceAllocationStrategy::OnDemand);
    config.cranelift_debug_verifier(true);
    // config.memory_init_cow(false);
    config.static_memory_forced(true);

    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);

    wasmtime_wasi::preview2::command::add_to_linker(&mut linker)?;

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdio();
    let mut table = Table::new();
    let wasi = wasi_builder.build(&mut table)?;

    let ctx = Ctx {
        table,
        wasi,
    };

    let mut store = Store::new(&engine, ctx);
    // store.limiter_async(|ctx| ctx as &mut dyn ResourceLimiterAsync);

    let component = Component::from_file(&engine, Path::new("/home/vigoo/projects/zivergetech/golem/integration-tests/src/it/wasm/shopping-cart.wasm"))?;
    // let component = Component::from_file(&engine, Path::new("/home/vigoo/projects/zivergetech/golem/integration-tests/src/it/wasm/python-1.wasm"))?;

    let instance_pre = linker.instantiate_pre(&component)?;
    let instance = instance_pre.instantiate_async(&mut store).await?;

    let (initialize_cart, add_item, get_cart_contents) = {
        let mut exports = instance.exports(&mut store);
        let mut api = exports.instance("golem:it/api").ok_or(anyhow!("cannot find golem:it/api"))?;
        (api.func("initialize-cart").ok_or(anyhow!("cannot find initialize-cart"))?,
         api.func("add-item").ok_or(anyhow!("cannot find add-item"))?,
         api.func("get-cart-contents").ok_or(anyhow!("cannot find get-cart-contents"))?)
    };


    let input4 = vec![];
    let mut output4: Vec<Val> = vec![Val::Bool(true)];

    get_cart_contents.call_async(&mut store, &input4, &mut output4).await?;
    get_cart_contents.post_return_async(&mut store).await?;

    println!("Contents of the new instance: {output4:?}");
    //let _snapshot = instance.snapshot(&mut store)?;

    println!("Restoring memory from snapshot");
    instance.restore(&mut store, snapshot)?;
    println!("Restored");

    dump_table(&store.data().table);

    let input5 = vec![];
    let mut output5: Vec<Val> = vec![Val::Bool(true)];

    get_cart_contents.call_async(&mut store, &input5, &mut output5).await?;
    get_cart_contents.post_return_async(&mut store).await?;

    println!("Contents of the new instance: {output5:?}");

    Ok(())
}

fn dump_table(table: &Table) {
    println!("Table[");
    for entry in table.snapshot() {
        println!("  {:?}", entry);
    }
    println!("Table]")
}

async fn async_main() -> Result<()> {
    env_logger::init();

    let snapshot = run_first().await?;
    println!("----------------------------------------------------------------------");
    run_second(snapshot).await?;
    Ok(())
}
