use std::io;

fn main() {
  let name = String::new();
  io::stdin::readline(name).unwrap();
  println!("Hello, World! {}", name);
}