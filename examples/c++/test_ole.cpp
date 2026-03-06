#include <iostream>

int main() {
    // Disable syncing with stdio for maximum output throughput
    std::ios_base::sync_with_stdio(false);
    while (true) {
        std::cout << "Flooding the output buffer with junk data...\n";
    }
    return 0;
}