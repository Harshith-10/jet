fn main() {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    loop {
        let chunk = vec![0u8; 10 * 1024 * 1024];
        chunks.push(chunk);
    }
}
