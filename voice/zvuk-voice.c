/* zvuk-voice — локальные голосовые команды плееру «Звук» на Яндекс Станции.
 *
 * Почему отдельный бинарник: модели и библиотеки прошивки — 32-битные (ARMv7),
 * а плеер zvukd собирается 64-битным, и загрузить их в него нельзя.
 *
 * Что делает:
 *   1) подключается к локальному каналу maind (127.0.0.1:9875) и подписывается на звук
 *      (кадр {252:{1:""}, 253:159});
 *   2) из входящих кадров берёт обработанный микрофонный поток: kind=158, канал vqe_0,
 *      16 кГц, моно, 16 бит;
 *   3) режет его по 640 байт (20 мс) и кормит штатным споттером прошивки
 *      (libyandex_evil.so + libtflite.so + libiconv.so) с командной моделью;
 *   4) на распознанное слово запускает /data/local/bin/zvukd с нужной командой.
 *
 * В облако ничего не уходит, поэтому Алиса на эти слова не реагирует.
 *
 * Запуск:  zvuk-voice [--monitor]
 *   --monitor — только печатать распознанное, ничего не запускать.
 */

#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <math.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <unistd.h>

#define STATION_IPC "127.0.0.1"
#define STATION_PORT 9875
#define COMMAND_MODEL "/system/vendor/quasar/spotter_models/command"
#define FRAME_BYTES 640 /* 20 мс, 16 кГц, s16 моно */
#define MAX_FRAME (1 << 20)

/* ---------- споттер прошивки ---------- */

struct spotter {
  void *iconv, *tflite, *library, *conf, *detector;
  const char *(*error_message)(const void *);
  void (*error_free)(void **);
  void *(*conf_read)(const char *, void **);
  uint32_t (*sample_rate)(const void *, void **);
  void *(*create)(const void *, uint32_t, uint32_t, void **);
  void *(*create_multi)(const void *, uint32_t, uint32_t, const int32_t *, void **);
  int32_t (*change_state)(void *, int32_t, int32_t);
  uint32_t (*required_channels)(const void *, void **);
  void (*submit)(void *, const void *, size_t, void **);
  int32_t (*next_event)(void *, void **);
  int32_t (*phrase_id)(void *, void **);
  const char *(*phrase)(void *, void **);
  void (*destroy)(void *, void **);
  void (*conf_free)(void *, void **);
};

static int vendor_error(struct spotter *s, void **error, char *out, size_t size) {
  if (!*error) return 0;
  const char *message = s->error_message(*error);
  snprintf(out, size, "spotter: %s", message ? message : "vendor error");
  s->error_free(error);
  *error = NULL;
  return -1;
}

#define LOAD(member, symbol)                                                   \
  do {                                                                         \
    *(void **)(&s->member) = dlsym(s->library, symbol);                        \
    if (!s->member) {                                                          \
      snprintf(error, size, "нет символа %s", symbol);                         \
      goto fail;                                                               \
    }                                                                          \
  } while (0)

/* Копия конфига модели с выключенными фильтром частот и блокировщиком TTS —
   без этого модель активации не стартует (приём из hu553in/yandex-station-chatgpt-voice). */
static char *model_overlay(const char *stock) {
  char flags_path[512], dir[] = "/tmp/zvuk-model-XXXXXX", line[256];
  snprintf(flags_path, sizeof(flags_path), "%s/flags.txt", stock);
  FILE *in = fopen(flags_path, "r");
  if (!in) return NULL;
  if (!mkdtemp(dir)) { fclose(in); return NULL; }
  char out_path[512];
  snprintf(out_path, sizeof(out_path), "%s/flags.txt", dir);
  FILE *out = fopen(out_path, "w");
  if (!out) { fclose(in); return NULL; }
  while (fgets(line, sizeof(line), in)) {
    if (strstr(line, "freq-filter-enabled true")) fputs("freq-filter-enabled false\n", out);
    else if (strstr(line, "use-tts-blocker true")) fputs("use-tts-blocker false\n", out);
    else fputs(line, out);
  }
  fclose(in);
  fclose(out);
  const char *files[] = {"acoustic_model.nnet", "words.txt"};
  for (int i = 0; i < 2; i++) {
    char from[512], to[512], target[512];
    snprintf(from, sizeof(from), "%s/%s", stock, files[i]);
    snprintf(to, sizeof(to), "%s/%s", dir, files[i]);
    snprintf(target, sizeof(target), "%s/%s", stock, files[i]);
    if (symlink(target, to) != 0) { /* файла может не быть — не критично */ }
    (void)from;
  }
  return strdup(dir);
}

static struct spotter *spotter_open(const char *model, char *error, size_t size) {
  struct spotter *s = calloc(1, sizeof(*s));
  if (!s) return NULL;
  s->iconv = dlopen("/system/vendor/quasar/libs/libiconv.so", RTLD_NOW | RTLD_GLOBAL);
  s->tflite = dlopen("/system/vendor/quasar/libs/libtflite.so", RTLD_NOW | RTLD_GLOBAL);
  s->library = dlopen("/system/vendor/quasar/libs/libyandex_evil.so", RTLD_NOW | RTLD_LOCAL);
  if (!s->iconv || !s->tflite || !s->library) {
    snprintf(error, size, "не загрузились библиотеки прошивки: %s", dlerror());
    goto fail;
  }
  LOAD(error_message, "spotter_error_get_message");
  LOAD(error_free, "spotter_error_free");
  LOAD(conf_read, "spotter_conf_read");
  LOAD(sample_rate, "spotter_conf_native_sample_rate");
  LOAD(create, "spotter_create");
  LOAD(create_multi, "spotter_create_multichannel");
  *(void **)(&s->change_state) = dlsym(s->library, "spotter_change_phrase_state");
  if (!s->change_state) {
    snprintf(error, size, "нет символа spotter_change_phrase_state");
    goto fail;
  }
  LOAD(required_channels, "spotter_conf_get_required_channel_count");
  LOAD(submit, "spotter_submit_raw_data");
  LOAD(next_event, "spotter_get_next_event");
  LOAD(phrase_id, "spotter_get_current_phrase_id");
  LOAD(phrase, "spotter_get_current_phrase");
  LOAD(destroy, "spotter_destroy");
  LOAD(conf_free, "spotter_conf_free");

  void *err = NULL;
  char *overlay = model_overlay(model);
  s->conf = s->conf_read(overlay ? overlay : model, &err);
  if (vendor_error(s, &err, error, size)) goto fail;
  uint32_t rate = s->sample_rate(s->conf, &err);
  if (vendor_error(s, &err, error, size)) goto fail;
  if (rate != 16000) {
    snprintf(error, size, "неожиданная частота споттера: %u", rate);
    goto fail;
  }
  /* Каналы задаются только многоканальным созданием: оно принимает массив номеров.
     Простой spotter_create каналов не передаёт вовсе, и модель падает с
     «channel count doesn't match» (проверено на модели активации). */
  uint32_t channels_needed = s->required_channels(s->conf, &err);
  if (vendor_error(s, &err, error, size)) goto fail;
  if (channels_needed == 0) channels_needed = 1;
  int32_t channels[8];
  for (uint32_t i = 0; i < 8; i++) channels[i] = (int32_t)i;
  s->detector = s->create_multi(s->conf, rate, channels_needed, channels, &err);
  if (vendor_error(s, &err, error, size)) goto fail;
  /* Включаем все фразы модели: без этого детектор молчит (в библиотеке есть отдельный
     вызов состояния фразы, по умолчанию фразы, видимо, выключены). */
  (void)s->change_state; /* вызов состояния фраз роняет модель — не используем */
  return s;
fail:
  if (s->library) dlclose(s->library);
  if (s->tflite) dlclose(s->tflite);
  if (s->iconv) dlclose(s->iconv);
  free(s);
  return NULL;
}

/* ---------- минимальный protobuf ---------- */

struct field {
  uint64_t num;
  uint64_t varint;
  const uint8_t *bytes;
  size_t len;
  int is_varint;
};

static int read_varint(const uint8_t *data, size_t size, size_t *pos, uint64_t *out) {
  *out = 0;
  for (int shift = 0; shift < 64; shift += 7) {
    if (*pos >= size) return -1;
    uint8_t b = data[(*pos)++];
    *out |= (uint64_t)(b & 0x7f) << shift;
    if (!(b & 0x80)) return 0;
  }
  return -1;
}

/* Перебирает поля сообщения. Возвращает 1, если поле прочитано, 0 — конец, -1 — ошибка. */
static int next_field(const uint8_t *data, size_t size, size_t *pos, struct field *f) {
  if (*pos >= size) return 0;
  uint64_t key;
  if (read_varint(data, size, pos, &key) < 0) return -1;
  f->num = key >> 3;
  int wire = (int)(key & 7);
  if (wire == 0) {
    if (read_varint(data, size, pos, &f->varint) < 0) return -1;
    f->is_varint = 1;
    return 1;
  }
  if (wire == 2) {
    uint64_t len;
    if (read_varint(data, size, pos, &len) < 0) return -1;
    if (*pos + len > size) return -1;
    f->bytes = data + *pos;
    f->len = (size_t)len;
    *pos += (size_t)len;
    f->is_varint = 0;
    return 1;
  }
  return -1;
}

/* ---------- канал maind ---------- */

static int station_connect(void) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) return -1;
  struct sockaddr_in addr;
  memset(&addr, 0, sizeof(addr));
  addr.sin_family = AF_INET;
  addr.sin_port = htons(STATION_PORT);
  if (inet_pton(AF_INET, STATION_IPC, &addr.sin_addr) != 1) { close(fd); return -1; }
  if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) { close(fd); return -1; }
  return fd;
}

static void put_varint(uint8_t *out, size_t *pos, uint64_t v) {
  while (v >= 0x80) { out[(*pos)++] = (uint8_t)(v | 0x80); v >>= 7; }
  out[(*pos)++] = (uint8_t)v;
}

static void put_tag(uint8_t *out, size_t *pos, uint64_t num, int wire) {
  put_varint(out, pos, (num << 3) | (uint64_t)wire);
}

/* Подписка на звук: {252:{1:" "}, 253:159} в 4-байтовой рамке длины. */
static int subscribe_audio(int fd) {
  uint8_t body[32];
  size_t pos = 0;
  put_tag(body, &pos, 252, 2); /* внешнее поле 252 — вложенное сообщение */
  put_varint(body, &pos, 2);   /* его длина */
  put_tag(body, &pos, 1, 2);   /* внутри: поле 1 — пустые байты */
  put_varint(body, &pos, 0);
  put_tag(body, &pos, 253, 0); /* тип сообщения */
  put_varint(body, &pos, 159); /* 159 = запрос звука */

  uint8_t frame[64];
  frame[0] = (uint8_t)(pos & 0xff);
  frame[1] = (uint8_t)((pos >> 8) & 0xff);
  frame[2] = (uint8_t)((pos >> 16) & 0xff);
  frame[3] = (uint8_t)((pos >> 24) & 0xff);
  memcpy(frame + 4, body, pos);
  return write(fd, frame, pos + 4) == (ssize_t)(pos + 4) ? 0 : -1;
}

static int read_exact(int fd, uint8_t *buf, size_t size) {
  size_t done = 0;
  while (done < size) {
    ssize_t n = read(fd, buf + done, size - done);
    if (n <= 0) return -1;
    done += (size_t)n;
  }
  return 0;
}

/* ---------- действия ---------- */

struct command {
  const char *word; /* как в words.txt командной модели */
  const char *action;
};

static const struct command commands[] = {
    {"играй", "flow"},     {"продолжи", "resume"}, {"дальше", "next"},
    {"следующий", "next"}, {"стоп", "stop"},        {"пауза", "pause"},
    {"хватит", "stop"},
};

static void run_action(const char *action) {
  char line[128];
  snprintf(line, sizeof(line), "/data/local/bin/zvukd %s >/dev/null 2>&1", action);
  printf("zvuk-voice: слово → %s\n", action);
  fflush(stdout);
  if (system(line) != 0) printf("zvuk-voice: команда не выполнилась\n");
}

static const char *action_for(const char *phrase) {
  for (size_t i = 0; i < sizeof(commands) / sizeof(commands[0]); i++) {
    if (strcmp(commands[i].word, phrase) == 0) return commands[i].action;
  }
  return NULL;
}

/* ---------- главный цикл ---------- */

int main(int argc, char **argv) {
  int monitor = 0;
  const char *model = COMMAND_MODEL;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--monitor") == 0) monitor = 1;
    if (strcmp(argv[i], "--model") == 0 && i + 1 < argc) model = argv[++i];
  }

  char error[512];
  struct spotter *spotter = spotter_open(model, error, sizeof(error));
  if (!spotter) {
    fprintf(stderr, "zvuk-voice: %s\n", error);
    return 1;
  }
  printf("zvuk-voice: споттер открыт%s\n", monitor ? " (только наблюдение)" : "");
  fflush(stdout);

  /* Источник звука — собственный захват с микрофона станции: 16 кГц, моно, s16.
     Канал maind на этой модели звук не отдаёт (проверено), а arecord работает. */
  const char *capture =
      "/usr/bin/arecord -D hw:0,1 -r 16000 -f S16_LE -c 1 -t raw -q -";
  uint8_t frame[FRAME_BYTES];
  long feeds = 0;

  for (;;) {
    FILE *mic = popen(capture, "r");
    if (!mic) {
      fprintf(stderr, "zvuk-voice: не удалось запустить захват микрофона\n");
      sleep(2);
      continue;
    }
    printf("zvuk-voice: слушаю микрофон\n");
    fflush(stdout);

    int16_t boosted[FRAME_BYTES / 2];
    while (fread(frame, 1, FRAME_BYTES, mic) == FRAME_BYTES) {
      void *err = NULL;
      /* Микрофон отдаётся сырым ADC-потоком и очень тихим: нормализуем уровень,
         иначе командная модель не срабатывает. */
      const int16_t *in = (const int16_t *)frame;
      int count = FRAME_BYTES / 2;
      long double energy = 0;
      for (int i = 0; i < count; i++) {
        long double v = in[i];
        energy += v * v;
      }
      double rms = count ? (double)sqrtl(energy / count) : 0;
      double gain = rms > 1 ? 2500.0 / rms : 1.0;
      if (gain < 1.0) gain = 1.0;
      if (gain > 120.0) gain = 120.0;
      for (int i = 0; i < count; i++) {
        long v = (long)(in[i] * gain);
        if (v > 32000) v = 32000;
        if (v < -32000) v = -32000;
        boosted[i] = (int16_t)v;
      }
      spotter->submit(spotter->detector, boosted, FRAME_BYTES, &err);
      if (vendor_error(spotter, &err, error, sizeof(error))) {
        fprintf(stderr, "zvuk-voice: %s\n", error);
        break;
      }
      feeds++;
      if (feeds % 250 == 0) {
        printf("zvuk-voice: работаю, обработано %ld кадров звука\n", feeds);
        fflush(stdout);
      }
      for (int k = 0; k < 16; k++) {
        int32_t event = spotter->next_event(spotter->detector, &err);
        if (vendor_error(spotter, &err, error, sizeof(error))) break;
        if (event == 0) break;
        if (event != 2 && event != 5) continue;
        const char *phrase = spotter->phrase(spotter->detector, &err);
        if (vendor_error(spotter, &err, error, sizeof(error))) break;
        printf("zvuk-voice: услышал «%s»\n", phrase ? phrase : "?");
        fflush(stdout);
        if (phrase && !monitor) {
          const char *action = action_for(phrase);
          if (action) run_action(action);
        }
      }
    }
    pclose(mic);
    printf("zvuk-voice: захват прервался, перезапускаю\n");
    fflush(stdout);
    sleep(1);
  }
}
