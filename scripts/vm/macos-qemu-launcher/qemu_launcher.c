// dlopen launcher for the QEMU dylibs bundled inside UTM.app (macOS 12 / Intel).
//
// Homebrew no longer ships Monterey bottles for qemu, and building from source
// drags in a 37-package dependency tree. UTM 4.6.x (last release supporting
// macOS 12) bundles QEMU 9.1.2 as dylib frameworks; this launcher dlopens them
// so plain `qemu-system-x86_64` / `qemu-img` binaries appear on PATH.
//
// Install: see README.md next to this file.

#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libgen.h>

#define QEMU_DIR "/usr/local/lib/qemu-utm"
#define SOFTMMU QEMU_DIR "/Frameworks/qemu-x86_64-softmmu.framework/qemu-x86_64-softmmu"
#define IMG     QEMU_DIR "/Frameworks/qemu-img.framework/Versions/A/qemu-img"
#define SHARE   QEMU_DIR "/share"

// softmmu dylibs export the modular API instead of main().
static int run_softmmu(int argc, char **argv) {
    void *h = dlopen(SOFTMMU, RTLD_NOW | RTLD_GLOBAL);
    if (!h) { fprintf(stderr, "qemu-system-x86_64: %s\n", dlerror()); return 1; }

    int  (*qemu_init)(int, char **) = dlsym(h, "qemu_init");
    void (*qemu_main_loop)(void)    = dlsym(h, "qemu_main_loop");
    void (*qemu_cleanup)(void)      = dlsym(h, "qemu_cleanup");
    if (!qemu_init || !qemu_main_loop || !qemu_cleanup) {
        fprintf(stderr, "qemu-system-x86_64: missing symbols: %s\n", dlerror());
        return 1;
    }

    // Inject firmware search path right after argv[0] (SeaBIOS etc.).
    char **nargv = malloc(sizeof(char *) * (argc + 3));
    if (!nargv) return 1;
    int n = 0;
    nargv[n++] = argv[0];
    nargv[n++] = (char *)"-L";
    nargv[n++] = (char *)SHARE;
    for (int i = 1; i < argc; i++) nargv[n++] = argv[i];
    nargv[n] = NULL;

    int rc = qemu_init(n, nargv);
    if (rc == 0) {
        qemu_main_loop();
        qemu_cleanup();
    }
    return rc;
}

// qemu-img dylib exports main(); forward argv verbatim.
static int run_img(int argc, char **argv) {
    void *h = dlopen(IMG, RTLD_NOW | RTLD_GLOBAL);
    if (!h) { fprintf(stderr, "qemu-img: %s\n", dlerror()); return 1; }
    int (*real_main)(int, char **) = dlsym(h, "main");
    if (!real_main) { fprintf(stderr, "qemu-img: no main: %s\n", dlerror()); return 1; }
    return real_main(argc, argv);
}

int main(int argc, char **argv) {
    char *prog = strdup(argv[0]);
    const char *name = basename(prog);
    int is_img = strstr(name, "qemu-img") != NULL;
    free(prog);
    return is_img ? run_img(argc, argv) : run_softmmu(argc, argv);
}
