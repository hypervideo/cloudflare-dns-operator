pub(crate) fn id() -> String {
    let charset = "1234567890abcdefghijklmnopqrstuvwxyz";
    random_string::generate(10, charset)
}
