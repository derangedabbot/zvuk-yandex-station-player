/* mic-tap — отдаёт копию микрофонного звука наружу, не отбирая его у Алисы.
 *
 * Зачем: микрофон на колонке монопольно держит процесс `maind --platformd`,
 * второй слушатель получает «занято». Эта вставка грузится в тот же процесс и
 * дублирует то, что он прочитал из микрофона, в именованный канал (FIFO).
 * Потребитель (наш детектор слова) читает канал и ничего не отбирает.
 *
 * Безопасность:
 *   - вставка ничего не делает в процессах, где нет `platformd` в командной строке;
 *   - при отсутствии канала чтения запись молча пропускается;
 *   - при ошибке любого шага вызывается настоящая функция — поведение процесса
 *     не меняется.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define TAP_PATH "/data/local/tmp/mic.fifo"
#define LOG_PATH "/data/local/tmp/mic-tap.log"

typedef long (*readi_fn)(void *, void *, unsigned long);

static readi_fn real_readi = NULL;
static int resolve_logged = 0;

static void note(const char *msg) {
  int fd = open(LOG_PATH, O_WRONLY | O_CREAT | O_APPEND, 0644);
  if (fd >= 0) {
    write(fd, msg, strlen(msg));
    close(fd);
  }
}

/* Ищем настоящую функцию тремя способами: иначе её процесс останется без звука,
   а значит и без микрофона — так уже было, когда вставка возвращала ошибку. */
static readi_fn resolve(const char *name) {
  readi_fn fn = (readi_fn)dlsym(RTLD_NEXT, name);
  if (!fn) {
    void *lib = dlopen("libasound.so.2", RTLD_LAZY | RTLD_NOLOAD);
    if (!lib) lib = dlopen("/usr/lib/libasound.so.2", RTLD_LAZY);
    if (lib) fn = (readi_fn)dlsym(lib, name);
  }
  if (!fn) {
    void *self = dlopen(NULL, RTLD_LAZY);
    if (self) fn = (readi_fn)dlsym(self, name);
  }
  if (!resolve_logged) {
    resolve_logged = 1;
    note(fn ? "mic-tap: настоящая функция найдена\n" : "mic-tap: НЕ НАЙДЕНА настоящая функция\n");
  }
  return fn;
}
static int target = -1; /* -1: не проверяли, 0: чужой процесс, 1: наш */
static int tap_fd = -1;
static unsigned long frames_total = 0;
static unsigned long bytes_total = 0;

/* Наш ли это процесс: смотрим командную строку. */
static int is_target(void) {
  if (target >= 0) return target;
  target = 0;
  int fd = open("/proc/self/cmdline", O_RDONLY);
  if (fd >= 0) {
    char buf[512];
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n > 0) {
      buf[n] = 0;
      for (ssize_t i = 0; i < n; i++)
        if (buf[i] == 0) buf[i] = ' ';
      if (strstr(buf, "platformd")) target = 1;
    }
  }
  return target;
}

static void tap_write(const void *data, size_t len) {
  if (tap_fd < 0) {
    tap_fd = open(TAP_PATH, O_WRONLY | O_NONBLOCK);
    if (tap_fd < 0) return; /* никто не слушает — это нормально */
  }
  ssize_t w = write(tap_fd, data, len);
  if (w < 0) {
    close(tap_fd);
    tap_fd = -1;
    return;
  }
  bytes_total += (unsigned long)w;
}

/* Первый вызов: пишем в журнал, что вставка загрузилась в нужный процесс. */
static void announce(void) {
  static int done = 0;
  if (done) return;
  done = 1;
  int fd = open(LOG_PATH, O_WRONLY | O_CREAT | O_APPEND, 0644);
  if (fd >= 0) {
    const char msg[] = "mic-tap: вставка загружена в platformd\n";
    ssize_t r = write(fd, msg, sizeof(msg) - 1);
    (void)r;
    close(fd);
  }
}

long snd_pcm_readi(void *pcm, void *buffer, unsigned long frames) {
  if (!real_readi) real_readi = resolve("snd_pcm_readi");
  if (!real_readi) return -EBADFD; /* лучше вернуть ошибку, чем падать */
  long rc = real_readi(pcm, buffer, frames);
  if (rc > 0 && is_target()) {
    announce();
    frames_total += (unsigned long)rc;
    /* Считаем, что формат — 16 бит: основной режим микрофона на этой колонке. */
    tap_write(buffer, (size_t)rc * 2);
  }
  return rc;
}

/* Некоторые сборки читают через mmap — дублируем и этот путь. */
long snd_pcm_mmap_readi(void *pcm, void *buffer, unsigned long frames) {
  typedef long (*fn_t)(void *, void *, unsigned long);
  static fn_t real = NULL;
  if (!real) real = (fn_t)resolve("snd_pcm_mmap_readi");
  if (!real) return -EBADFD;
  long rc = real(pcm, buffer, frames);
  if (rc > 0 && is_target()) {
    announce();
    frames_total += (unsigned long)rc;
    tap_write(buffer, (size_t)rc * 2);
  }
  return rc;
}
