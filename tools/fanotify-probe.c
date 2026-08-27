// Probe for the kernel capability GOnedrive's M2 files-on-demand depends on.
//
// It creates a sparse "placeholder" file, marks it for pre-content events, and
// then reads it from a child process. If the kernel delivers FAN_PRE_ACCESS
// before the read completes, and this program can fill the content and answer
// FAN_ALLOW without deadlocking, the fanotify HSM path is viable on this
// machine and M2 can proceed as planned. If not, M2 falls back to FUSE.
//
// Deliberately plain C with no dependencies: this exercises raw syscalls and
// is meant to be readable by anyone auditing what runs as root.
//
//   gcc -O2 -Wall -o fanotify-probe fanotify-probe.c
//   sudo ./fanotify-probe /path/on/ext4/probe.bin
//
// Requires Linux 6.14+ and CAP_SYS_ADMIN.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/fanotify.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fanotify.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define FILE_SIZE (1024 * 1024)
#define FILL_BYTE 0x5a

static long blocks_used(const char *path) {
    struct stat st;
    if (stat(path, &st) != 0) return -1;
    return (long)st.st_blocks;
}

// Create a placeholder: real file, real size, zero blocks on disk.
static int make_placeholder(const char *path) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open"); return -1; }
    if (ftruncate(fd, FILE_SIZE) != 0) { perror("ftruncate"); close(fd); return -1; }
    // Punching a hole over the whole range is what makes `du` report 0 while
    // `ls` still shows the real size — the same illusion CFAPI gives on Windows.
    if (fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, FILE_SIZE) != 0) {
        perror("fallocate(PUNCH_HOLE)");
        close(fd);
        return -1;
    }
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <path on a local filesystem>\n", argv[0]);
        return 2;
    }
    const char *path = argv[1];

    if (make_placeholder(path) != 0) return 1;
    printf("placeholder created: size=%d blocks=%ld\n", FILE_SIZE, blocks_used(path));

    int fan = fanotify_init(FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC, O_RDWR | O_LARGEFILE);
    if (fan < 0) {
        perror("fanotify_init(FAN_CLASS_PRE_CONTENT)");
        fprintf(stderr, "  -> needs CAP_SYS_ADMIN and Linux 6.14+\n");
        return 1;
    }

    // An inode mark, not a mount mark: only dehydrated files should be
    // intercepted. A mount mark would route every read on the filesystem
    // through this process.
    if (fanotify_mark(fan, FAN_MARK_ADD, FAN_PRE_ACCESS, AT_FDCWD, path) != 0) {
        perror("fanotify_mark(FAN_PRE_ACCESS)");
        return 1;
    }
    printf("inode mark installed\n");

    pid_t child = fork();
    if (child == 0) {
        // The reader. It must block until the parent fills the content.
        int fd = open(path, O_RDONLY);
        if (fd < 0) { perror("child open"); _exit(1); }
        unsigned char buf[4096];
        ssize_t n = pread(fd, buf, sizeof(buf), 0);
        if (n < 0) { perror("child pread"); _exit(1); }
        for (ssize_t i = 0; i < n; i++) {
            if (buf[i] != FILL_BYTE) {
                fprintf(stderr, "child: byte %zd was 0x%02x, expected 0x%02x "
                                "(read raced ahead of hydration)\n", i, buf[i], FILL_BYTE);
                _exit(1);
            }
        }
        printf("child: read %zd bytes, all hydrated\n", n);
        _exit(0);
    }

    // The event loop. In the real helper this thread never performs IO; here
    // the fill is inline only because the probe has a single event to handle.
    char events[4096];
    ssize_t len = read(fan, events, sizeof(events));
    if (len <= 0) { perror("read(fanotify)"); return 1; }

    for (struct fanotify_event_metadata *e = (struct fanotify_event_metadata *)events;
         FAN_EVENT_OK(e, len); e = FAN_EVENT_NEXT(e, len)) {

        printf("event: mask=0x%llx fd=%d pid=%d\n",
               (unsigned long long)e->mask, e->fd, e->pid);

        if (e->mask & FAN_PRE_ACCESS) {
            // Range info tells us which part of the file is actually needed,
            // so a real client can fetch a range instead of the whole object.
            struct fanotify_event_info_header *hdr =
                (struct fanotify_event_info_header *)((char *)e + e->metadata_len);
            if ((char *)hdr < (char *)e + e->event_len &&
                hdr->info_type == FAN_EVENT_INFO_TYPE_RANGE) {
                struct fanotify_event_info_range *r =
                    (struct fanotify_event_info_range *)hdr;
                printf("  range: offset=%llu count=%llu\n",
                       (unsigned long long)r->offset, (unsigned long long)r->count);
            } else {
                printf("  range: not reported\n");
            }

            // Fill through the event's own fd: it has already passed the
            // permission check, so writing here does not re-enter the hook.
            unsigned char *fill = malloc(FILE_SIZE);
            if (!fill) { perror("malloc"); return 1; }
            memset(fill, FILL_BYTE, FILE_SIZE);
            ssize_t written = pwrite(e->fd, fill, FILE_SIZE, 0);
            free(fill);
            if (written != FILE_SIZE) {
                perror("pwrite(hydrate)");
                struct fanotify_response deny = {
                    .fd = e->fd, .response = FAN_DENY };
                (void)!write(fan, &deny, sizeof(deny));
                return 1;
            }
            printf("  hydrated %zd bytes\n", written);
        }

        struct fanotify_response resp = { .fd = e->fd, .response = FAN_ALLOW };
        if (write(fan, &resp, sizeof(resp)) != sizeof(resp)) {
            perror("write(FAN_ALLOW)");
            return 1;
        }
        close(e->fd);
    }

    int status = 0;
    waitpid(child, &status, 0);
    printf("blocks after hydration: %ld\n", blocks_used(path));

    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
        printf("\nPASS: fanotify HSM works here. M2 can use the placeholder backend.\n");
        return 0;
    }
    printf("\nFAIL: the reader did not get hydrated content. M2 must use FUSE.\n");
    return 1;
}
