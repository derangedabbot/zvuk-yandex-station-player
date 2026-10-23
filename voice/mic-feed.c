/* mic-feed — берёт микрофонный звук из локального канала Алисы (127.0.0.1:9875).
 *
 * Зачем: настоящий микрофон (`hw:0,2`) монопольно держит `maind --platformd`,
 * поэтому открыть его вторым нельзя. Но её же канал отдаёт обработанный звук
 * отдельным потоком: подписка кадром {252:{1:""}, 253:159}, ответы — кадры
 * `253=158`, внутри канал с именем `vqe_0`, PCM 16 кГц моно (s16le).
 * Приём взят из hu553in/yandex-station-chatgpt-voice (internal/voice/station_ipc.go).
 *
 * Использование:  mic-feed [/tmp/mic.fifo|--stdout]
 */

#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#define ALICE_HOST "127.0.0.1"
#define ALICE_PORT 9875
#define MAX_FRAME (10 * 1024 * 1024)

static volatile sig_atomic_t stop_requested = 0;
static void on_signal(int sig) { (void)sig; stop_requested = 1; }

/* ---- разбор protobuf без библиотеки ---- */

static int read_varint(const unsigned char *d, size_t len, size_t *pos, uint64_t *out) {
  uint64_t v = 0;
  int shift = 0;
  while (*pos < len && shift < 64) {
    unsigned char b = d[(*pos)++];
    v |= (uint64_t)(b & 0x7f) << shift;
    if (!(b & 0x80)) { *out = v; return 1; }
    shift += 7;
  }
  return 0;
}

static void put_varint(unsigned char **p, uint64_t v) {
  while (v >= 128) { *(*p)++ = (unsigned char)((v & 0x7f) | 0x80); v >>= 7; }
  *(*p)++ = (unsigned char)v;
}

/* Подписка на звук: {252: {1: ""}, 253: 159} с длиной кадра впереди. */
static size_t build_subscription(unsigned char *out) {
  unsigned char body[16];
  unsigned char *b = body;
  *b++ = 0xE2; *b++ = 0x0F; /* поле 252, тип bytes */
  *b++ = 0x02;
  *b++ = 0x0A; *b++ = 0x00; /* внутри: поле 1, пустые байты */
  *b++ = 0xE8; *b++ = 0x0F; /* поле 253, тип varint */
  *b++ = 0x9F; *b++ = 0x01; /* значение 159 */
  size_t n = (size_t)(b - body);
  out[0] = (unsigned char)(n & 0xff);
  out[1] = (unsigned char)((n >> 8) & 0xff);
  out[2] = 0;
  out[3] = 0;
  memcpy(out + 4, body, n);
  return n + 4;
}

/* Достаём из кадра поле kind (253) и payload (252). */
static int envelope(const unsigned char *d, size_t len, uint64_t *kind,
                    const unsigned char **payload, size_t *payload_len) {
  size_t pos = 0;
  int have_kind = 0, have_payload = 0;
  while (pos < len) {
    uint64_t key;
    if (!read_varint(d, len, &pos, &key)) return 0;
    uint64_t num = key >> 3, type = key & 7;
    if (type == 0) {
      uint64_t v;
      if (!read_varint(d, len, &pos, &v)) return 0;
      if (num == 253) { *kind = v; have_kind = 1; }
    } else if (type == 2) {
      uint64_t n;
      if (!read_varint(d, len, &pos, &n)) return 0;
      if (pos + n > len) return 0;
      if (num == 252) { *payload = d + pos; *payload_len = (size_t)n; have_payload = 1; }
      pos += n;
    } else if (type == 1) { pos += 8; }
    else if (type == 5) { pos += 4; }
    else return 0;
  }
  return have_kind && have_payload;
}

/* Кадр звука (kind 158): {1: {2: имя канала, 3: pcm, 7: частота, 8: формат}} */
static long extract_audio(const unsigned char *d, size_t len, const char **name,
                          const unsigned char **pcm, size_t *pcm_len) {
  size_t pos = 0;
  while (pos < len) {
    uint64_t key;
    if (!read_varint(d, len, &pos, &key)) break;
    uint64_t num = key >> 3, type = key & 7;
    if (type == 2) {
      uint64_t n;
      if (!read_varint(d, len, &pos, &n)) break;
      if (pos + n > len) break;
      if (num == 1) {
        const unsigned char *c = d + pos;
        size_t clen = (size_t)n, cp = 0;
        const char *nm = NULL;
        const unsigned char *data = NULL;
        size_t data_len = 0;
        uint64_t rate = 0, fmt = 0;
        int have_rate = 0, have_fmt = 0;
        while (cp < clen) {
          uint64_t k2;
          if (!read_varint(c, clen, &cp, &k2)) break;
          uint64_t n2 = k2 >> 3, t2 = k2 & 7;
          if (t2 == 2) {
            uint64_t m;
            if (!read_varint(c, clen, &cp, &m)) break;
            if (cp + m > clen) break;
            if (n2 == 2) nm = (const char *)(c + cp);
            if (n2 == 3) { data = c + cp; data_len = (size_t)m; }
            cp += m;
          } else if (t2 == 0) {
            uint64_t v;
            if (!read_varint(c, clen, &cp, &v)) break;
            if (n2 == 7) { rate = v; have_rate = 1; }
            if (n2 == 8) { fmt = v; have_fmt = 1; }
          } else break;
        }
        if (nm && data && have_rate && have_fmt && rate == 16000 && fmt == 1 &&
            strcmp(nm, "vqe_0") == 0) {
          *name = nm;
          *pcm = data;
          *pcm_len = data_len;
          return (long)data_len;
        }
      }
      pos += n;
    } else if (type == 0) {
      uint64_t v;
      if (!read_varint(d, len, &pos, &v)) break;
    } else if (type == 1) { pos += 8; }
    else if (type == 5) { pos += 4; }
    else break;
  }
  return 0;
}

int main(int argc, char **argv) {
  signal(SIGTERM, on_signal);
  signal(SIGINT, on_signal);

  const char *sink = argc > 1 ? argv[1] : "--stdout";
  int out_fd = STDOUT_FILENO;
  if (strcmp(sink, "--stdout") != 0) {
    remove(sink);
    if (mkfifo(sink, 0666) != 0 && errno != EEXIST) perror("mkfifo");
    out_fd = open(sink, O_WRONLY | O_NONBLOCK); /* ждём читателя без блокировки */
  }

  int sock = socket(AF_INET, SOCK_STREAM, 0);
  if (sock < 0) { perror("socket"); return 1; }
  struct sockaddr_in addr;
  memset(&addr, 0, sizeof(addr));
  addr.sin_family = AF_INET;
  addr.sin_port = htons(ALICE_PORT);
  addr.sin_addr.s_addr = inet_addr(ALICE_HOST);
  if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
    perror("connect 9875");
    return 1;
  }

  unsigned char sub[32];
  size_t sub_len = build_subscription(sub);
  if (send(sock, sub, sub_len, 0) != (ssize_t)sub_len) {
    perror("подписка не ушла");
    return 1;
  }
  fprintf(stderr, "mic-feed: подписался на звук (vqe_0, 16 кГц)\n");

  long frames = 0, audio_frames = 0;
  unsigned long long total = 0;
  while (!stop_requested) {
    unsigned char hdr[4];
    ssize_t got = recv(sock, hdr, 4, MSG_WAITALL);
    if (got != 4) break;
    uint32_t size = (uint32_t)hdr[0] | ((uint32_t)hdr[1] << 8) | ((uint32_t)hdr[2] << 16) |
                    ((uint32_t)hdr[3] << 24);
    if (size == 0 || size > MAX_FRAME) break;
    unsigned char *buf = malloc(size);
    if (!buf) break;
    if (recv(sock, buf, size, MSG_WAITALL) != (ssize_t)size) { free(buf); break; }
    frames++;
    uint64_t kind = 0;
    {   /* какие типы кадров вообще приходят — для диагностики */
      static uint64_t seen[32];
      static int seen_n = 0;
      uint64_t k0 = 0; const unsigned char *pp = NULL; size_t pl = 0;
      if (envelope(buf, size, &k0, &pp, &pl)) {
        int known = 0;
        for (int i = 0; i < seen_n; i++) if (seen[i] == k0) known = 1;
        if (!known && seen_n < 32) { seen[seen_n++] = k0; fprintf(stderr, "mic-feed: новый тип кадра %llu\n", (unsigned long long)k0); }
      }
    }
    const unsigned char *payload = NULL;
    size_t payload_len = 0;
    if (envelope(buf, size, &kind, &payload, &payload_len) && kind == 158) {
      const char *name = NULL;
      const unsigned char *pcm = NULL;
      size_t pcm_len = 0;
      if (extract_audio(payload, payload_len, &name, &pcm, &pcm_len) > 0) {
        audio_frames++;
        total += pcm_len;
        if (out_fd < 0) out_fd = open(sink, O_WRONLY | O_NONBLOCK); /* читатель появился */
        if (out_fd >= 0 && write(out_fd, pcm, pcm_len) < 0) {
          close(out_fd);
          out_fd = -1; /* читателя нет — просто пропускаем */
        }
        if (audio_frames % 100 == 0) {
          fprintf(stderr, "mic-feed: кадров звука %ld, %.1f с\n", audio_frames,
                  (double)total / 32000.0);
        }
      }
    }
    free(buf);
  }
  fprintf(stderr, "mic-feed: итог — кадров %ld, со звуком %ld, %.1f с\n", frames, audio_frames,
          (double)total / 32000.0);
  close(sock);
  return 0;
}
