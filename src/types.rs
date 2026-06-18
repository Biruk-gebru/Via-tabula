pub fn is_tombstone(value: &[u8]) -> bool {
    value == b"__TOMBSTONE__"
}
