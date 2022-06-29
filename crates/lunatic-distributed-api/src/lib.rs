use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use lunatic_common_api::{get_memory, IntoTrap};
use lunatic_distributed::{
    distributed::message::{Spawn, Val},
    DistributedCtx,
};
use lunatic_process::message::{DataMessage, Message};
use lunatic_process_api::ProcessCtx;
use wasmtime::{Caller, Linker, ResourceLimiter, Trap};

// Register the process APIs to the linker
pub fn register<T>(linker: &mut Linker<T>) -> Result<()>
where
    T: DistributedCtx + ProcessCtx<T> + Send + ResourceLimiter + 'static,
    for<'a> &'a T: Send,
{
    linker.func_wrap("lunatic::distributed", "nodes_count", nodes_count)?;
    linker.func_wrap("lunatic::distributed", "get_nodes", get_nodes)?;
    linker.func_wrap("lunatic::distributed", "node_id", node_id)?;
    linker.func_wrap("lunatic::distributed", "module_id", module_id)?;
    linker.func_wrap8_async("lunatic::distributed", "spawn", spawn)?;
    linker.func_wrap2_async("lunatic::distributed", "send", send)?;
    linker.func_wrap3_async(
        "lunatic::distributed",
        "send_receive_skip_search",
        send_receive_skip_search,
    )?;
    Ok(())
}

// Returns count of registered nodes
fn nodes_count<T: DistributedCtx>(caller: Caller<T>) -> u32 {
    caller
        .data()
        .distributed()
        .map(|d| d.control.node_count())
        .unwrap_or(0) as u32
}

// Copy node ids to memory TODO doc
fn get_nodes<T: DistributedCtx>(
    mut caller: Caller<T>,
    nodes_ptr: u32,
    nodes_len: u32,
) -> Result<u32, Trap> {
    let memory = get_memory(&mut caller)?;
    let node_ids = caller
        .data()
        .distributed()
        .map(|d| d.control.node_ids())
        .unwrap_or_else(|_| vec![]);
    let copy_nodes_len = node_ids.len().min(nodes_len as usize);
    memory
        .data_mut(&mut caller)
        .get_mut(
            nodes_ptr as usize
                ..(nodes_ptr as usize + std::mem::size_of::<u64>() * copy_nodes_len as usize),
        )
        .or_trap("lunatic::distributed::get_nodes::memory")?
        .copy_from_slice(unsafe { node_ids[..copy_nodes_len].align_to::<u8>().1 });
    Ok(copy_nodes_len as u32)
}

// TODO docs!!
// Spawns a new process using the passed in function inside a module as the entry point.
//
// If **link** is not 0, it will link the child and parent processes. The value of the **link**
// argument will be used as the link-tag for the child. This means, if the child traps the parent
// is going to get a signal back with the value used as the tag.
//
// If *config_id* or *module_id* have the value 0, the same module/config is used as in the
// process calling this function.
//
// The function arguments are passed as an array with the following structure:
// [0 byte = type ID; 1..17 bytes = value as u128, ...]
// The type ID follows the WebAssembly binary convention:
//  - 0x7F => i32
//  - 0x7E => i64
//  - 0x7B => v128
// If any other value is used as type ID, this function will trap.
//
// TODO add link and config support
//
// Returns:
// * 0 on success - The ID of the newly created process is written to **id_ptr**
// * 1 on error   - The error ID is written to **id_ptr**
//
// Traps:
// * If the module ID doesn't exist.
// * If the function string is not a valid utf8 string.
// * If the params array is in a wrong format.
// * If any memory outside the guest heap space is referenced.
#[allow(clippy::too_many_arguments)]
fn spawn<T>(
    mut caller: Caller<T>,
    node_id: u64,
    config_id: i64,
    module_id: u64,
    func_str_ptr: u32,
    func_str_len: u32,
    params_ptr: u32,
    params_len: u32,
    id_ptr: u32,
) -> Box<dyn Future<Output = Result<u32, Trap>> + Send + '_>
where
    T: DistributedCtx + ResourceLimiter + Send + 'static,
    for<'a> &'a T: Send,
{
    Box::new(async move {
        if !caller.data().can_spawn() {
            return Err(anyhow!("Process doesn't have permissions to spawn sub-processes").into());
        }
        let memory = get_memory(&mut caller)?;
        let func_str = memory
            .data(&caller)
            .get(func_str_ptr as usize..(func_str_ptr + func_str_len) as usize)
            .or_trap("lunatic::distributed::spawn::func_str")?;

        let function =
            std::str::from_utf8(func_str).or_trap("lunatic::distributed::spawn::func_str_utf8")?;

        let params = memory
            .data(&caller)
            .get(params_ptr as usize..(params_ptr + params_len) as usize)
            .or_trap("lunatic::distributed::spawn::params")?;

        let params = params
            .chunks_exact(17)
            .map(|chunk| {
                let value = u128::from_le_bytes(chunk[1..].try_into()?);
                let result = match chunk[0] {
                    0x7F => Val::I32(value as i32),
                    0x7E => Val::I64(value as i64),
                    0x7B => Val::V128(value),
                    _ => return Err(anyhow!("Unsupported type ID")),
                };
                Ok(result)
            })
            .collect::<Result<Vec<_>>>()?;

        let state = caller.data();

        let config = match config_id {
            -1 => state.config().clone(),
            config_id => Arc::new(
                caller
                    .data()
                    .config_resources()
                    .get(config_id as u64)
                    .or_trap("lunatic::process::spawn: Config ID doesn't exist")?
                    .clone(),
            ),
        };
        let config: Vec<u8> =
            bincode::serialize(config.as_ref()).map_err(|_| anyhow!("Error serializing config"))?;

        log::debug!("Spawn on node {node_id}, mod {module_id}, fn {function}, params {params:?}");

        let (proc_id, ret) = match state
            .distributed()?
            .distributed_client
            .spawn(
                node_id,
                Spawn {
                    environment_id: state.environment_id(),
                    function: function.to_string(),
                    module_id,
                    params,
                    config,
                },
            )
            .await
        {
            Ok(id) => (id, 0),
            Err(_) => (0, 1), // TODO errors
        };

        memory
            .write(&mut caller, id_ptr as usize, &proc_id.to_le_bytes())
            .or_trap("lunatic::distributed::spawn::write_id")?;

        Ok(ret)
    })
}

#[allow(clippy::too_many_arguments)]
fn send<T>(
    mut caller: Caller<T>,
    node_id: u64,
    process_id: u64,
) -> Box<dyn Future<Output = Result<(), Trap>> + Send + '_>
where
    T: DistributedCtx + ProcessCtx<T> + Send + 'static,
    for<'a> &'a T: Send,
{
    Box::new(async move {
        let message = caller
            .data_mut()
            .message_scratch_area()
            .take()
            .or_trap("lunatic::message::send::no_message")?;
        // TODO trap on non-empty resources
        if let Message::Data(DataMessage { tag, buffer, .. }) = message {
            let state = caller.data();
            state
                .distributed()?
                .distributed_client
                .message_process(node_id, state.environment_id(), process_id, tag, buffer)
                .await?;
        }
        Ok(())
    })
}

fn send_receive_skip_search<T>(
    mut caller: Caller<T>,
    node_id: u64,
    process_id: u64,
    timeout: u32,
) -> Box<dyn Future<Output = Result<u32, Trap>> + Send + '_>
where
    T: DistributedCtx + ProcessCtx<T> + Send + 'static,
    for<'a> &'a T: Send,
{
    Box::new(async move {
        let message = caller
            .data_mut()
            .message_scratch_area()
            .take()
            .or_trap("lunatic::message::send::no_message")?;

        let mut _tags = [0; 1];
        let tags = if let Some(tag) = message.tag() {
            _tags = [tag];
            Some(&_tags[..])
        } else {
            None
        };

        // TODO trap on non-empty resources
        if let Message::Data(DataMessage { tag, buffer, .. }) = message {
            let state = caller.data();
            state
                .distributed()?
                .distributed_client
                .message_process(node_id, state.environment_id(), process_id, tag, buffer)
                .await?;

            if let Some(message) = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(timeout as u64)), if timeout != 0 => None,
                message = caller.data_mut().mailbox().pop_skip_search(tags) => Some(message)
            } {
                // Put the message into the scratch area
                caller.data_mut().message_scratch_area().replace(message);
                Ok(0)
            } else {
                Ok(9027)
            }
        } else {
            // TODO err?
            Ok(9027)
        }
    })
}

// Returns ID of the node that the current process is running on
fn node_id<T: DistributedCtx>(caller: Caller<T>) -> u64 {
    caller
        .data()
        .distributed()
        .as_ref()
        .map(|d| d.node_id())
        .unwrap_or(0)
}

// Returns ID of the module that the current process is spawned from
fn module_id<T: DistributedCtx>(caller: Caller<T>) -> u64 {
    caller.data().module_id()
}
