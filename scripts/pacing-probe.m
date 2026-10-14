// Native, permission-free inventory and timer control for pacing.py.
// Build: clang -O2 -framework Foundation -framework Metal pacing-probe.m -o probe
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

static int compare(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

static NSDictionary *measure(bool critical, int count) {
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    double scale = (double)tb.numer / tb.denom;
    double *samples = calloc(count, sizeof(double));
    int queue = critical ? kqueue() : -1;
    if (!samples || (critical && queue < 0)) exit(2);
    for (int i = 0; i < count; ++i) {
        uint64_t start = mach_absolute_time();
        if (critical) {
            struct kevent64_s change = {
                .ident = 1, .filter = EVFILT_TIMER, .flags = EV_ADD | EV_ONESHOT,
                .fflags = NOTE_MACHTIME | NOTE_ABSOLUTE | NOTE_CRITICAL | NOTE_LEEWAY,
                .data = start + (uint64_t)(10000000 / scale),
                .ext = {0, (uint64_t)(2500000 / scale)},
            }, event;
            int result;
            do { result = kevent64(queue, &change, 1, &event, 1, 0, NULL); }
            while (result < 0 && errno == EINTR);
            if (result != 1 || (event.flags & EV_ERROR)) exit(3);
        } else {
            struct timespec remaining = {0, 10000000};
            while (nanosleep(&remaining, &remaining) && errno == EINTR) {}
        }
        samples[i] = (mach_absolute_time() - start) * scale / 1e6;
    }
    qsort(samples, count, sizeof(double), compare);
    NSDictionary *result = @{
        @"samples": @(count), @"requested_ms": @10,
        @"explicit_leeway_ms": critical ? @2.5 : (id)[NSNull null],
        @"elapsed_ms": @{@"p50": @(samples[(count - 1) / 2]),
                         @"p99": @(samples[(count * 99 + 99) / 100 - 1]),
                         @"max": @(samples[count - 1])},
        @"slop_ms": @{@"p50": @(samples[(count - 1) / 2] - 10),
                      @"p99": @(samples[(count * 99 + 99) / 100 - 1] - 10)}
    };
    free(samples);
    if (queue >= 0) close(queue);
    return result;
}

int main(void) {
    @autoreleasepool {
        task_category_policy_data_t category = {0};
        mach_msg_type_number_t count = TASK_CATEGORY_POLICY_COUNT;
        boolean_t defaults = FALSE;
        int policy_status = task_policy_get(mach_task_self(), TASK_CATEGORY_POLICY,
            (task_policy_t)&category, &count, &defaults);
        struct task_qos_policy qos = {0};
        count = TASK_QOS_POLICY_COUNT;
        defaults = FALSE;
        int qos_status = task_policy_get(mach_task_self(), TASK_BASE_QOS_POLICY,
            (task_policy_t)&qos, &count, &defaults);
        errno = 0;
        int background = getpriority(PRIO_DARWIN_PROCESS, 0), priority_error = errno;
        NSMutableDictionary *result = [@{
            @"pid": @(getpid()), @"ppid": @(getppid()),
            @"task_role": @(category.role), @"task_policy_status": @(policy_status),
            @"task_qos_status": @(qos_status),
            @"latency_tier": @(qos.task_latency_qos_tier),
            @"throughput_tier": @(qos.task_throughput_qos_tier),
            @"darwin_background": @(background), @"getpriority_errno": @(priority_error),
            @"initial_thread_qos": @(qos_class_self()),
        } mutableCopy];
        // Unlike system_profiler this identifies the actual Metal device in a VM.
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        result[@"metal"] = device ? @{@"name": device.name,
            @"unified_memory": @(device.hasUnifiedMemory)} : (id)[NSNull null];
        result[@"nanosleep_default"] = measure(false, 1000);
        result[@"qos_set_status"] = @(pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0));
        result[@"requested_thread_qos"] = @(qos_class_self());
        result[@"nanosleep_interactive_qos"] = measure(false, 1000);
        result[@"critical_timer"] = measure(true, 1000);
        NSData *data = [NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingPrettyPrinted error:NULL];
        fwrite(data.bytes, 1, data.length, stdout);
        putchar('\n');
    }
}
