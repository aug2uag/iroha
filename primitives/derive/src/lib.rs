mod socket_addr;

/// Convenience macro to concisely construct a [`SocketAddr`]
///
/// # Examples
/// ```
/// # use iroha_primitives_derive::socket_addr;
///
/// let localhost = socket_addr!(127.0.0.1:8080);
/// let remote = socket_addr!([2001:db8::1]:8080);
/// ```
#[proc_macro]
pub fn socket_addr(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    socket_addr::socket_addr_impl(input.into()).into()
}