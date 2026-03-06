#include <iostream>
#include <vector>
#include <algorithm>
#include <random>

int main() {
    std::vector<int> data(1000000);
    std::mt19937 rng(42);
    
    for (int& x : data) {
        x = rng();
    }
    
    std::sort(data.begin(), data.end());
    std::cout << "Sorted 1,000,000 integers." << std::endl;
    return 0;
}