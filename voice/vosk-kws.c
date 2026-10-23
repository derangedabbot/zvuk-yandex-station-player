/* vosk-kws — детектор одного слова («Салют») для Яндекс Станции.
 *
 * Зачем: голосовые команды через облако Алисы конфликтуют с ней самой за звук.
 * Отдельный офлайн-детектор ловит своё слово, не поднимая Алису и ничего не отправляя
 * в облако. Движок — Vosk (свободная лицензия Apache 2.0), модель русская компактная.
 *
 * Как работает:
 *   1) берём микрофон станции: arecord -D hw:0,1 -r 16000 -f S16_LE -c 1 -t raw;
 *   2) гоняем поток через распознаватель с грамматикой из одного слова;
 *   3) на совпадении запускаем команду плеера (по умолчанию flow — включить поток).
 *
 * Запуск:  vosk-kws [--monitor] [--word САЛЮТ] [--action flow]
 */

#define _POSIX_C_SOURCE 200809L
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "vosk_api.h"

#define SAMPLE_RATE 16000
#define CHUNK 3200 /* 100 мс, 16 кГц, s16 моно */

static volatile sig_atomic_t stop_requested = 0;

static void on_signal(int sig) {
  (void)sig;
  stop_requested = 1;
}

/* Приводим строку к нижнему регистру UTF-8 (для сравнения с распознанным текстом). */
static void lower(char *s) {
  for (unsigned char *p = (unsigned char *)s; *p; p++) {
    if (*p >= 'A' && *p <= 'Z') *p = (unsigned char)(*p - 'A' + 'a');
    if (*p == 0xD0 || *p == 0xD1) {
      /* кириллица в UTF-8: заглавные идут диапазонами 0xD0 0x90..0xAF и 0xD0 0xA0..0xAF */
      if (p[1] >= 0x80 && p[1] <= 0x9F) p[1] = (unsigned char)(p[1] + 0x20);
    }
  }
}

int main(int argc, char **argv) {
  int monitor = 0;
  const char *word = "салют";
  const char *action = "flow";

  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--monitor") == 0) monitor = 1;
    else if (strcmp(argv[i], "--word") == 0 && i + 1 < argc) word = argv[++i];
    else if (strcmp(argv[i], "--action") == 0 && i + 1 < argc) action = argv[++i];
  }

  const char *model_dir = getenv("VOSK_MODEL");
  if (!model_dir) model_dir = "/data/local/opt/vosk/model";

  signal(SIGTERM, on_signal);
  signal(SIGINT, on_signal);

  VoskModel *model = vosk_model_new(model_dir);
  if (!model) {
    fprintf(stderr, "vosk-kws: не загрузилась модель из %s\n", model_dir);
    return 1;
  }

  char grammar[128];
  snprintf(grammar, sizeof(grammar), "[\"%s\", \"[unk]\"]", word);
  VoskRecognizer *rec = vosk_recognizer_new_grm(model, SAMPLE_RATE, grammar);
  if (!rec) {
    fprintf(stderr, "vosk-kws: не создался распознаватель (грамматика: %s)\n", grammar);
    vosk_model_free(model);
    return 1;
  }

  printf("vosk-kws: слушаю слово «%s»%s\n", word, monitor ? " (только наблюдение)" : "");
  fflush(stdout);

  /* Источник звука: по умолчанию микрофон через arecord, либо канал mic.fifo,
     куда пишет вставка в процесс Алисы (когда микрофон занят ею). */
  const char *fifo = getenv("KWS_FIFO");
  char capture[256];
  if (fifo && *fifo) {
    snprintf(capture, sizeof(capture), "cat %s", fifo);
  } else {
    snprintf(capture, sizeof(capture),
             "/usr/bin/arecord -D hw:0,1 -r 16000 -f S16_LE -c 1 -t raw -q -");
  }
  char buf[CHUNK];
  long chunks = 0;

  while (!stop_requested) {
    FILE *mic = popen(capture, "r");
    if (!mic) {
      fprintf(stderr, "vosk-kws: не запустился захват микрофона\n");
      sleep(2);
      continue;
    }
    while (!stop_requested && fread(buf, 1, sizeof(buf), mic) > 0) {
      int done = vosk_recognizer_accept_waveform(rec, buf, (int)sizeof(buf));
      const char *text = done ? vosk_recognizer_result(rec) : vosk_recognizer_partial_result(rec);
      chunks++;
      if (monitor && text && *text) {
        char flat[512];
        size_t j = 0;
        for (const char *q = text; *q && j < sizeof(flat) - 1; q++) {
          flat[j++] = (*q == '\n') ? ' ' : *q;
        }
        flat[j] = 0;
        printf("vosk-kws: %s\n", flat);
        fflush(stdout);
      }
      if (text && *text) {
        char copy[512];
        snprintf(copy, sizeof(copy), "%s", text);
        lower(copy);
        if (strstr(copy, word)) {
          printf("vosk-kws: услышал «%s» (кусков: %ld)\n", text, chunks);
          fflush(stdout);
          if (!monitor) {
            char line[256];
            snprintf(line, sizeof(line),
                     "/data/local/bin/zvukd %s >/dev/null 2>&1", action);
            if (system(line) != 0) fprintf(stderr, "vosk-kws: команда не выполнилась\n");
          }
          vosk_recognizer_reset(rec);
        }
      }
      if (done) vosk_recognizer_reset(rec);
    }
    pclose(mic);
    if (!stop_requested) {
      printf("vosk-kws: захват прервался, перезапускаю\n");
      fflush(stdout);
      sleep(1);
    }
  }

  vosk_recognizer_free(rec);
  vosk_model_free(model);
  return 0;
}
