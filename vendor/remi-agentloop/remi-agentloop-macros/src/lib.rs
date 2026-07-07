use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, FnArg, ItemFn, Pat, ReturnType, Type};

/// `#[tool]` proc-macro: generates a `Tool` impl from an async fn.
///
/// ## Usage
/// ```ignore
/// #[tool]
/// /// Search the web for information.
/// async fn web_search(query: String) -> String {
///     // ... implementation
/// }
/// ```
///
/// This generates a struct `WebSearch` that implements the `Tool` trait.
/// The doc comment becomes the `description()`.
/// Each parameter becomes a JSON Schema property.
#[proc_macro_attribute]
pub fn tool(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match tool_impl(func) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Detect what kind of return type the user wrote.
enum ReturnMode {
    /// Simple value like String, f64, etc. — auto-wrap with to_string() + Content::text()
    SimpleValue,
    /// `Content` — rich multimodal result, wrap directly in ToolOutput::Result
    ContentValue,
    /// `ToolResult<SomeValue>` — user handles Output/Interrupt, we map inner to stream
    ToolResultValue,
    /// `ToolResult<impl Stream<Item = ToolOutput>>` — full control, pass through
    ToolResultStream,
}

fn detect_return_mode(ret: &ReturnType) -> ReturnMode {
    match ret {
        ReturnType::Default => ReturnMode::SimpleValue,
        ReturnType::Type(_, ty) => {
            let ts = quote! { #ty }.to_string().replace(" ", "");
            if ts.starts_with("ToolResult") || ts.contains("::ToolResult") {
                // Check if the inner type mentions Stream or ToolOutput stream
                if ts.contains("Stream") {
                    ReturnMode::ToolResultStream
                } else {
                    ReturnMode::ToolResultValue
                }
            } else if ts == "Content"
                || ts.ends_with("::Content")
                || ts.ends_with("::types::Content")
            {
                ReturnMode::ContentValue
            } else {
                ReturnMode::SimpleValue
            }
        }
    }
}

fn tool_impl(func: ItemFn) -> syn::Result<TokenStream2> {
    let func_name = &func.sig.ident;
    let func_name_str = func_name.to_string();

    // Struct name: snake_case → PascalCase
    let struct_name = to_pascal_case(&func_name_str);
    let struct_ident = syn::Ident::new(&struct_name, func_name.span());

    // Extract doc comments as description
    let description = extract_doc_comments(&func.attrs);

    // Extract parameters — separating special params (ctx, resume) from tool args
    let extracted = extract_params_v2(&func.sig.inputs)?;

    // Build JSON Schema properties (only for tool args, not ctx/resume)
    let schema_props = build_schema_props(&extracted.tool_params);
    let required_fields: Vec<&str> = extracted
        .tool_params
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();

    // Build argument extraction in execute()
    let arg_extractions = build_arg_extractions(&extracted.tool_params);

    let vis = &func.vis;
    let block = &func.block;
    let return_mode = detect_return_mode(&func.sig.output);

    // For ToolResult returns, we use an untyped let binding;
    // for simple returns we annotate with the declared type.
    let ret_type = match &func.sig.output {
        ReturnType::Default => quote! { String },
        ReturnType::Type(_, t) => quote! { #t },
    };

    let required_json = required_fields.iter().map(|s| quote! { #s });

    // Build bindings for special params that the user declared
    let ctx_binding = if extracted.has_ctx {
        quote! { let ctx = _ctx; }
    } else {
        quote! {}
    };
    let resume_binding = if extracted.has_resume {
        quote! { let resume = _resume; }
    } else {
        quote! {}
    };

    // Build the body of the execute() async block depending on return mode
    let execute_body = match return_mode {
        ReturnMode::SimpleValue => {
            // User returns a simple value — wrap in ToolResult::Output + stream
            quote! {
                async move {
                    #ctx_binding
                    #resume_binding
                    #(#arg_extractions)*
                    let result: #ret_type = { #block };
                    let result_str = result.to_string();
                    Ok(::remi_agentloop::tool::ToolResult::Output(
                        ::async_stream::stream! {
                            yield ::remi_agentloop::tool::ToolOutput::Result(
                                ::remi_agentloop::types::Content::text(result_str)
                            );
                        }
                    ))
                }
            }
        }
        ReturnMode::ContentValue => {
            // User returns Content directly — wrap in ToolOutput::Result
            quote! {
                async move {
                    #ctx_binding
                    #resume_binding
                    #(#arg_extractions)*
                    let result: #ret_type = { #block };
                    Ok(::remi_agentloop::tool::ToolResult::Output(
                        ::async_stream::stream! {
                            yield ::remi_agentloop::tool::ToolOutput::Result(result);
                        }
                    ))
                }
            }
        }
        ReturnMode::ToolResultValue => {
            // User returns ToolResult<SomeValue> — map Output's inner to stream
            quote! {
                async move {
                    #ctx_binding
                    #resume_binding
                    #(#arg_extractions)*
                    let result: #ret_type = { #block };
                    match result {
                        ::remi_agentloop::tool::ToolResult::Output(val) => {
                            let result_str = val.to_string();
                            Ok(::remi_agentloop::tool::ToolResult::Output(
                                ::async_stream::stream! {
                                    yield ::remi_agentloop::tool::ToolOutput::Result(
                                        ::remi_agentloop::types::Content::text(result_str)
                                    );
                                }
                            ))
                        }
                        ::remi_agentloop::tool::ToolResult::Interrupt(req) => {
                            Ok(::remi_agentloop::tool::ToolResult::Interrupt(req))
                        }
                    }
                }
            }
        }
        ReturnMode::ToolResultStream => {
            // User returns ToolResult<impl Stream<Item = ToolOutput>> — pass through
            // Don't annotate the type (impl Trait not allowed in let bindings)
            quote! {
                async move {
                    #ctx_binding
                    #resume_binding
                    #(#arg_extractions)*
                    let result = { #block };
                    Ok(result)
                }
            }
        }
    };

    Ok(quote! {
        #[derive(Debug, Clone)]
        #vis struct #struct_ident;

        impl #struct_ident {
            pub fn new() -> Self { Self }
        }

        impl ::remi_agentloop::tool::Tool for #struct_ident {
            fn name(&self) -> &str {
                #func_name_str
            }

            fn description(&self) -> &str {
                #description
            }

            fn parameters_schema(&self) -> ::serde_json::Value {
                ::serde_json::json!({
                    "type": "object",
                    "properties": {
                        #(#schema_props),*
                    },
                    "required": [#(#required_json),*]
                })
            }

            fn execute(
                &self,
                arguments: ::serde_json::Value,
                _resume: ::std::option::Option<::remi_agentloop::types::ResumePayload>,
                _ctx: &::remi_agentloop::tool::ToolContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<
                    ::remi_agentloop::tool::ToolResult<
                        impl ::futures::Stream<Item = ::remi_agentloop::tool::ToolOutput>
                    >,
                    ::remi_agentloop::error::AgentError
                >
            > {
                #execute_body
            }
        }
    })
}

fn extract_doc_comments(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("doc") {
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    lines.push(s.value().trim().to_string());
                }
            }
        }
    }
    lines.join(" ")
}

/// Result of parameter extraction — separates special framework params from tool arguments.
struct ExtractedParams {
    /// Normal tool arguments that become JSON Schema properties.
    tool_params: Vec<(String, String)>,
    /// Whether the user declared a `ctx` / `context` parameter (ToolContext).
    has_ctx: bool,
    /// Whether the user declared a `resume` parameter (Option<ResumePayload>).
    has_resume: bool,
}

/// Check if a type looks like `ToolContext`, `&ToolContext`, etc.
fn is_tool_context_type(ty: &Type) -> bool {
    let ts = quote! { #ty }.to_string().replace(" ", "");
    ts == "ToolContext"
        || ts == "&ToolContext"
        || ts.ends_with("::ToolContext")
        || ts.ends_with("::ToolContext")
}

/// Check if a type looks like `Option<ResumePayload>`, etc.
fn is_resume_type(ty: &Type) -> bool {
    let ts = quote! { #ty }.to_string().replace(" ", "");
    ts.contains("ResumePayload")
}

fn extract_params_v2(
    inputs: &syn::punctuated::Punctuated<FnArg, syn::token::Comma>,
) -> syn::Result<ExtractedParams> {
    let mut tool_params = Vec::new();
    let mut has_ctx = false;
    let mut has_resume = false;

    for input in inputs {
        match input {
            FnArg::Receiver(_) => {} // skip self
            FnArg::Typed(pt) => {
                let name = match pt.pat.as_ref() {
                    Pat::Ident(pi) => pi.ident.to_string(),
                    _ => continue,
                };

                // Detect special params by name + type
                if (name == "ctx" || name == "context") && is_tool_context_type(pt.ty.as_ref()) {
                    has_ctx = true;
                    continue;
                }
                if name == "resume" && is_resume_type(pt.ty.as_ref()) {
                    has_resume = true;
                    continue;
                }

                let type_str = type_to_json_schema_type(pt.ty.as_ref());
                tool_params.push((name, type_str));
            }
        }
    }

    Ok(ExtractedParams {
        tool_params,
        has_ctx,
        has_resume,
    })
}

fn type_to_json_schema_type(ty: &Type) -> String {
    let ts = quote! { #ty }.to_string();
    let ts = ts.replace(" ", "");
    match ts.as_str() {
        "String" | "&str" => "string".to_string(),
        "i64" | "i32" | "i16" | "i8" | "u64" | "u32" | "u16" | "u8" | "usize" | "isize" => {
            "integer".to_string()
        }
        "f64" | "f32" => "number".to_string(),
        "bool" => "boolean".to_string(),
        _ => "string".to_string(),
    }
}

fn build_schema_props(params: &[(String, String)]) -> Vec<TokenStream2> {
    params
        .iter()
        .map(|(name, ty)| {
            quote! {
                #name: { "type": #ty }
            }
        })
        .collect()
}

fn build_arg_extractions(params: &[(String, String)]) -> Vec<TokenStream2> {
    params
        .iter()
        .map(|(name, ty)| {
            let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
            let extraction = match ty.as_str() {
                "integer" => quote! {
                    let #ident: i64 = arguments[#name].as_i64()
                        .ok_or_else(|| ::remi_agentloop::error::AgentError::ToolExecution {
                            tool_name: stringify!(#ident).to_string(),
                            message: format!("missing or invalid integer argument: {}", #name),
                        })?;
                },
                "number" => quote! {
                    let #ident: f64 = arguments[#name].as_f64()
                        .ok_or_else(|| ::remi_agentloop::error::AgentError::ToolExecution {
                            tool_name: stringify!(#ident).to_string(),
                            message: format!("missing or invalid number argument: {}", #name),
                        })?;
                },
                "boolean" => quote! {
                    let #ident: bool = arguments[#name].as_bool()
                        .ok_or_else(|| ::remi_agentloop::error::AgentError::ToolExecution {
                            tool_name: stringify!(#ident).to_string(),
                            message: format!("missing or invalid boolean argument: {}", #name),
                        })?;
                },
                _ => quote! {
                    let #ident: String = arguments[#name].as_str()
                        .ok_or_else(|| ::remi_agentloop::error::AgentError::ToolExecution {
                            tool_name: stringify!(#ident).to_string(),
                            message: format!("missing or invalid string argument: {}", #name),
                        })?
                        .to_string();
                },
            };
            extraction
        })
        .collect()
}

fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect()
}
