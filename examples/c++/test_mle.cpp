#include <iostream>
#include <vector>

int main() {
    std::vector<std::vector<char>> memory_hog;
    while (true) {
        // Allocate and write to 10MB of memory
        memory_hog.push_back(std::vector<char>(10 * 1024 * 1024, 1));
    }
    return 0;
}