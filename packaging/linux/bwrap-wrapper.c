#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define MAX_ARGUMENT_BYTES (1024 * 1024)

static void fail(const char *message) {
    fprintf(stderr, "Brick sandbox runtime: %s\n", message);
    exit(1);
}

// The GTK bundler changes WebKit's /usr prefix to ././. Bubblewrap changes
// directory inside its sandbox, so resolve that exact generated prefix against
// this executable's AppDir. Never expand shell syntax or change other paths.
static char *relocate(const char *root, char *argument) {
    if (strncmp(argument, "././", 4)) return argument;
    size_t length = strlen(root) + strlen(argument + 4) + 2;
    char *result = malloc(length);
    if (!result) fail("out of memory");
    snprintf(result, length, "%s/%s", root, argument + 4);
    return result;
}

static void write_all(int fd, const char *bytes, size_t length) {
    while (length) {
        ssize_t written = write(fd, bytes, length);
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) fail("could not prepare sandbox arguments");
        bytes += written;
        length -= (size_t)written;
    }
}

// The upstream /usr rewrite also relocates WebKit's standard read-only
// runtime mounts. Retain those original mounts (e.g. host glibc behind /lib's
// /usr/lib symlink) alongside AppDir libraries. Only restore exact existing
// read-only source=destination triples from WebKit's own argument block.
static size_t retain_system_runtime(int output, char *argument, char *next, char *limit) {
    if (strcmp(argument, "--ro-bind-try") || next >= limit) return 0;
    char *end = memchr(next, '\0', (size_t)(limit - next));
    if (!end || end + 1 >= limit) return 0;
    char *destination = end + 1;
    if (!memchr(destination, '\0', (size_t)(limit - destination)) || strcmp(next, destination) || strncmp(next, "././", 4)) return 0;
    const char *relative = next + 4;
    while (*relative == '/') relative++;
    const char *allowed[] = { "lib", "lib64", "lib32", "local/lib", "local/lib64", "local/lib32", "share", "local/share" };
    for (size_t index = 0; index < sizeof(allowed) / sizeof(allowed[0]); index++) {
        if (strcmp(relative, allowed[index])) continue;
        char path[64];
        snprintf(path, sizeof(path), "/usr/%s", relative);
        write_all(output, "--ro-bind-try", sizeof("--ro-bind-try"));
        write_all(output, path, strlen(path) + 1);
        write_all(output, path, strlen(path) + 1);
        return sizeof("--ro-bind-try") + 2 * (strlen(path) + 1);
    }
    return 0;
}

// WebKit passes mount arguments as NUL-delimited strings in a sealed memfd.
// Replace that descriptor in this child only, preserving its number and every
// non-path byte. In particular, seccomp and synchronization FDs stay intact.
static void relocate_fd(const char *root, const char *number) {
    char *end;
    errno = 0;
    long parsed = strtol(number, &end, 10);
    if (errno || !*number || *end || parsed < 3 || parsed > INT_MAX)
        fail("invalid sandbox argument descriptor");
    int fd = (int)parsed;
    struct stat status;
    if (fstat(fd, &status) || !S_ISREG(status.st_mode) || status.st_size <= 0 || status.st_size > MAX_ARGUMENT_BYTES)
        fail("invalid sandbox argument data");
    size_t length = (size_t)status.st_size;
    char *bytes = malloc(length);
    if (!bytes) fail("out of memory");
    size_t read_count = 0;
    while (read_count < length) {
        ssize_t count = pread(fd, bytes + read_count, length - read_count, (off_t)read_count);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) fail("could not read sandbox arguments");
        read_count += (size_t)count;
    }
    int replacement = memfd_create("brick-sandbox-arguments", MFD_ALLOW_SEALING);
    if (replacement < 0) fail("could not create sandbox arguments");
    size_t output_length = 0;
    for (size_t offset = 0; offset < length;) {
        char *terminator = memchr(bytes + offset, '\0', length - offset);
        if (!terminator) fail("unterminated sandbox argument");
        char *argument = bytes + offset;
        if (!strcmp(argument, "--args")) fail("nested sandbox argument descriptors are unsupported");
        output_length += retain_system_runtime(replacement, argument, terminator + 1, bytes + length);
        char *translated = relocate(root, argument);
        size_t translated_length = strlen(translated) + 1;
        output_length += translated_length;
        if (output_length > 4 * MAX_ARGUMENT_BYTES) fail("oversized sandbox arguments");
        write_all(replacement, translated, translated_length);
        if (translated != argument) free(translated);
        offset = (size_t)(terminator - bytes) + 1;
    }
    free(bytes);
    if (lseek(replacement, 0, SEEK_SET) < 0 ||
        fcntl(replacement, F_ADD_SEALS, F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE) < 0 ||
        dup2(replacement, fd) < 0)
        fail("could not seal sandbox arguments");
    close(replacement);
}

int main(int argc, char **argv) {
    if (argc < 1) fail("missing argument vector");
    char root[PATH_MAX + 1];
    ssize_t length = readlink("/proc/self/exe", root, PATH_MAX);
    if (length < 0 || length >= PATH_MAX) fail("could not locate AppDir");
    root[length] = '\0';
    for (int level = 0; level < 2; level++) {
        char *separator = strrchr(root, '/');
        if (!separator) fail("invalid AppDir path");
        *separator = '\0';
    }
    char **arguments = calloc((size_t)argc + 1, sizeof(char *));
    if (!arguments) fail("out of memory");
    const char *suffix = "/bin/brick-bwrap";
    arguments[0] = malloc(strlen(root) + strlen(suffix) + 1);
    if (!arguments[0]) fail("out of memory");
    strcpy(arguments[0], root);
    strcat(arguments[0], suffix);
    for (int index = 1; index < argc; index++) {
        if (!strcmp(argv[index], "--args")) {
            if (index + 1 >= argc) fail("missing sandbox argument descriptor");
            relocate_fd(root, argv[index + 1]);
        }
        arguments[index] = relocate(root, argv[index]);
    }
    execv(arguments[0], arguments);
    fail("could not start bubblewrap");
}
