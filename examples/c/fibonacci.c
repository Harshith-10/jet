#include <stdio.h>

long long fib(int n) {
    if (n <= 1) return n;
    return fib(n - 1) + fib(n - 2);
}

int main() {
    int n = 40; // Adjust higher to push CPU limits
    printf("Fibonacci of %d is %lld\n", n, fib(n));
    return 0;
}