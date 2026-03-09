fn fib(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    fib(n - 1) + fib(n - 2)
}

fn main() {
    let result = fib(40);
    println!("Fibonacci is {}", result);
}
