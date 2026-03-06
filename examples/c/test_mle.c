#include <stdlib.h>
#include <string.h>

int main() {
    while (1) {
        // Allocate 10MB at a time
        void *p = malloc(1024 * 1024 * 10);
        if (p == NULL) break;
        memset(p, 0, 1024 * 1024 * 10); // Force physical allocation
    }
    return 0;
}