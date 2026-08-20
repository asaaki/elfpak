/* Uses a library that lives outside every directory the loader searches. */

#include <stdio.h>

int vendor_value(void);

int main(void) {
    printf("vendor value=%d\n", vendor_value());
    return 0;
}
