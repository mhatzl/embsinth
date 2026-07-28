use proc_macro::TokenStream;
use syn::{Expr, LitStr, Stmt, parse_quote};

/// Attribute to create a test that captures logs per test case.
///
/// ## Usage
///
/// ```ignore
/// #[test]
/// fn my_test() {
///   // your test code as usual...
/// }
/// ```
#[proc_macro_attribute]
pub fn test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let Ok(parsed_item) = syn::parse::<syn::Item>(item) else {
        panic!("Failed to parse the item `req_test` was set on.")
    };
    let syn::Item::Fn(mut fn_item) = parsed_item else {
        panic!("Attribute `req_test` can only be set on a function.");
    };

    let mut expected_panic: Option<Expr> = None;

    for attrb in &fn_item.attrs {
        if attrb.path().is_ident("should_panic") {
            if expected_panic.is_some() {
                panic!("Not allowed to have more than one `should_panic` attribute.")
            }

            let _ = attrb.parse_nested_meta(|meta| {
                if meta.path.is_ident("expected") {
                    let value = meta.value()?;
                    let msg: LitStr = value.parse()?;
                    expected_panic =
                        Some(parse_quote!(embsinth::logger::ExpectedPanicMsg::Exact(#msg)));
                }

                Ok(())
            });

            if expected_panic.is_none() {
                expected_panic = Some(parse_quote!(
                    embsinth::logger::::ExpectedPanicMsg::Any
                ));
            }
        }
    }

    let test_attrb: syn::Attribute = parse_quote!(#[test]);
    fn_item.attrs.push(test_attrb);

    let fn_name = fn_item.sig.ident.clone();
    let orig_stmts = fn_item.block.stmts;
    let wrapped_stmts: Stmt = parse_quote! {
        {
            #(#orig_stmts);*
        }
    };

    fn_item.block.stmts = match expected_panic {
        Some(expected_msg) => {
            let log_init: Stmt = parse_quote!(
                embsinth::logger::test_case_start(core::any::type_name_of_val(&#fn_name), file!(), line!(), embsinth::logger::PanicHandling::ShouldPanic(#expected_msg));
            );
            vec![log_init, wrapped_stmts]
        }
        None => {
            let log_init: Stmt = parse_quote!(
                embsinth::logger::test_case_start(core::any::type_name_of_val(&#fn_name), file!(), line!(), embsinth::logger::PanicHandling::FailOnPanic);
            );
            let log_end: Stmt = parse_quote!(embsinth::logger::test_case_end(););
            vec![log_init, wrapped_stmts, log_end]
        }
    };

    quote::quote!(#fn_item).into()
}
