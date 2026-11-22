#import <mach/mach_time.h>

uint64_t now(void) {
    return mach_absolute_time();
}
