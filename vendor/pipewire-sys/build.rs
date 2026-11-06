// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

use quote::quote;
use std::env;
use std::path::PathBuf;

// External functions used by pinned pipewire-rs 0.10.1 and Isocast. An
// unlisted function is a compile error, not a late failure on the RT thread.
// SPA/core/registry interface methods are already dispatched by their vtables.
const FUNCTIONS: &str = "
pw_client_info_free pw_context_connect pw_context_connect_fd pw_context_destroy
pw_context_get_data_loop pw_context_get_properties pw_context_new pw_context_update_properties
pw_core_disconnect pw_data_loop_get_loop pw_deinit pw_device_info_free pw_factory_info_free
pw_init pw_link_info_free pw_loop_destroy pw_loop_new
pw_main_loop_destroy pw_main_loop_get_loop pw_main_loop_new pw_main_loop_quit pw_main_loop_run
pw_module_info_free pw_node_info_free pw_port_info_free pw_properties_clear pw_properties_copy
pw_properties_free pw_properties_get pw_properties_new pw_properties_new_dict pw_properties_new_string
pw_properties_set pw_proxy_add_listener pw_proxy_destroy pw_proxy_get_id pw_proxy_get_type
pw_stream_add_listener pw_stream_connect pw_stream_dequeue_buffer pw_stream_destroy pw_stream_disconnect
pw_stream_flush pw_stream_get_name pw_stream_get_node_id pw_stream_get_properties pw_stream_get_state
pw_stream_get_time pw_stream_get_time_n pw_stream_is_driving pw_stream_new pw_stream_queue_buffer
pw_stream_set_active pw_stream_set_control pw_stream_set_error pw_stream_trigger_process
pw_stream_update_params pw_thread_loop_accept pw_thread_loop_destroy pw_thread_loop_get_loop
pw_thread_loop_get_time pw_thread_loop_in_thread pw_thread_loop_lock pw_thread_loop_new
pw_thread_loop_signal pw_thread_loop_start pw_thread_loop_stop pw_thread_loop_timed_wait
pw_thread_loop_timed_wait_full pw_thread_loop_unlock pw_thread_loop_wait";

fn main() {
    let libpipewire = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libpipewire-0.3")
        .expect("Cannot find PipeWire headers");

    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed=wrapper.h");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());

    let builder = bindgen::Builder::default()
        .header("wrapper.h")
        // Tell cargo to invalidate the built crate whenever any of the
        // included header files changed.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .size_t_is_usize(true)
        .allowlist_function(format!(
            "({})",
            FUNCTIONS.split_whitespace().collect::<Vec<_>>().join("|")
        ))
        .dynamic_library_name("PwApi")
        .dynamic_link_require_all(true)
        .allowlist_type("pw_.*")
        .allowlist_var("pw_.*")
        .allowlist_var("PW_.*")
        // No imported mutable global variables, only header constants.
        .blocklist_item("pw_log_level|PW_LOG_TOPIC.*")
        .blocklist_function("spa_.*")
        .blocklist_type("spa_.*")
        .blocklist_item("spa_.*")
        .raw_line("pub(super) use spa_sys::*;")
        // Some definitions are missing from the generated bindings without this
        .clang_macro_fallback()
        .clang_macro_fallback_build_dir(&out_path);

    let builder = libpipewire
        .include_paths
        .iter()
        .fold(builder, |builder, l| {
            let arg = format!("-I{}", l.to_string_lossy());
            builder.clang_arg(arg)
        });

    let bindings = builder.generate().expect("Unable to generate bindings");

    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    // Retain pipewire-rs's module-level call syntax, including C variadics.
    // Read signatures from bindgen's own table: no hand-maintained C prototypes.
    let ast = syn::parse_file(&bindings.to_string()).unwrap();
    let table = ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Struct(s) if s.ident == "PwApi" => Some(s),
            _ => None,
        })
        .unwrap();
    let entries = table.fields.iter().filter_map(|field| {
        let name = field.ident.as_ref()?;
        if name == "__library" {
            return None;
        }
        let ty = &field.ty;
        assert!(
            matches!(ty, syn::Type::BareFn(_)),
            "unexpected PipeWire data symbol {name}"
        );
        Some(quote! { pub const #name: Function<#ty> = Function(|api| &api.#name); })
    });
    std::fs::write(out_path.join("calls.rs"), quote!(#(#entries)*).to_string()).unwrap();
}
