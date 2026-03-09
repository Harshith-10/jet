unsafe extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

fn main() {
    unsafe {
        let pid = fork();
        if pid < 0 {
            eprintln!("Fork failed");
            std::process::exit(1);
        }

        if pid == 0 {
            println!("Child success");
            std::process::exit(0);
        } else {
            let mut status = 0;
            let _ = waitpid(pid, &mut status, 0);
            println!("Parent success");
        }
    }
}
