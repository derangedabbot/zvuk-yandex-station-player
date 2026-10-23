/* frames-dump — показать структуру кадров локального канала Алисы (127.0.0.1:9875).
 *
 * Нужно, чтобы понять, что она транслирует наружу: состояния распознавания,
 * события своих команд, прочие сообщения. Печатает длину кадра, номера и типы
 * полей protobuf и первые байты содержимого.
 */

#define _POSIX_C_SOURCE 200809L
#include <arpa/inet.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static volatile sig_atomic_t stop_requested = 0;
static void on_signal(int sig) { (void)sig; stop_requested = 1; }

static int read_varint(const unsigned char *d, size_t len, size_t *pos, unsigned long long *out) {
  unsigned long long v = 0;
  int shift = 0;
  while (*pos < len && shift < 64) {
    unsigned char b = d[(*pos)++];
    v |= (unsigned long long)(b & 0x7f) << shift;
    if (!(b & 0x80)) { *out = v; return 1; }
    shift += 7;
  }
  return 0;
}

/* Разбор верхнего уровня: печатаем номер поля, тип и длину/значение. */
static void dump_fields(const unsigned char *d, size_t len, int depth) {
  size_t pos = 0;
  int shown = 0;
  while (pos < len && shown < 8) {
    unsigned long long key = 0;
    if (!read_varint(d, len, &pos, &key)) return;
    unsigned long long num = key >> 3, type = key & 7;
    if (type == 0) {
      unsigned long long v = 0;
      if (!read_varint(d, len, &pos, &v)) return;
      printf("%*sполе %llu = %llu\n", depth * 2, "", num, v);
    } else if (type == 2) {
      unsigned long long n = 0;
      if (!read_varint(d, len, &pos, &n)) return;
      if (pos + n > len) return;
      printf("%*sполе %llu: %llu байт:", depth * 2, "", num, n);
      for (unsigned long long i = 0; i < n && i < 24; i++) printf(" %02x", d[pos + i]);
      printf("%s\n", n > 24 ? " …" : "");
      pos += n;
    } else if (type == 1) { pos += 8; printf("%*sполе %llu: 8 байт\n", depth * 2, "", num); }
    else if (type == 5) { pos += 4; printf("%*sполе %llu: 4 байта\n", depth * 2, "", num); }
    else return;
    shown++;
  }
}

int main(void) {
  signal(SIGTERM, on_signal);
  signal(SIGINT, on_signal);

  int sock = socket(AF_INET, SOCK_STREAM, 0);
  if (sock < 0) { perror("socket"); return 1; }
  struct sockaddr_in addr;
  memset(&addr, 0, sizeof(addr));
  addr.sin_family = AF_INET;
  addr.sin_port = htons(9875);
  addr.sin_addr.s_addr = inet_addr("127.0.0.1");
  if (connect(sock, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
    perror("connect");
    return 1;
  }

  printf("frames-dump: подключился к 127.0.0.1:9875\n");
  fflush(stdout);

  long frames = 0;
  while (!stop_requested) {
    unsigned char hdr[4];
    ssize_t got = recv(sock, hdr, 4, MSG_WAITALL);
    if (got != 4) break;
    uint32_t size = (uint32_t)hdr[0] | ((uint32_t)hdr[1] << 8) | ((uint32_t)hdr[2] << 16) |
                    ((uint32_t)hdr[3] << 24);
    if (size == 0 || size > 512 * 1024) {
      printf("frames-dump: странная длина %u\n", size);
      break;
    }
    unsigned char *buf = malloc(size);
    if (!buf) break;
    got = recv(sock, buf, size, MSG_WAITALL);
    if (got != (ssize_t)size) { free(buf); break; }
    frames++;
    printf("\n== кадр %ld, %u байт\n", frames, size);
    dump_fields(buf, size, 1);
    fflush(stdout);
    free(buf);
  }
  printf("frames-dump: всего кадров %ld\n", frames);
  close(sock);
  return 0;
}
