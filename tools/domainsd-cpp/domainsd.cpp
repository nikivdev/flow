#include <arpa/inet.h>
#include <fcntl.h>
#ifdef __APPLE__
#include <launch.h>
#endif
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cctype>
#include <csignal>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <limits>
#include <mutex>
#include <optional>
#include <regex>
#include <sstream>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

namespace {

constexpr const char* kHeaderName = "X-Flow-Domainsd";
constexpr const char* kHeaderValue = "1";
constexpr size_t kMaxHeaderBytes = 1024 * 1024;
constexpr size_t kIoBufferSize = 16 * 1024;
constexpr size_t kDefaultPoolMaxIdlePerKey = 8;
constexpr size_t kDefaultPoolMaxIdleTotal = 256;
constexpr int kDefaultPoolIdleTimeoutMs = 15'000;
constexpr int kDefaultPoolMaxAgeMs = 120'000;
constexpr int kDefaultUpstreamConnectTimeoutMs = 10'000;
constexpr int kDefaultUpstreamIoTimeoutMs = 15'000;
constexpr int kDefaultClientIoTimeoutMs = 30'000;
constexpr int kDefaultMaxActiveClients = 128;
constexpr int kDefaultRouteReloadCheckIntervalMs = 100;

std::atomic<bool> g_running{true};
int g_listen_fd = -1;
std::string g_pidfile;
std::atomic<int> g_active_clients{0};
std::atomic<uint64_t> g_overload_rejections{0};
size_t g_pool_max_idle_per_key = kDefaultPoolMaxIdlePerKey;
size_t g_pool_max_idle_total = kDefaultPoolMaxIdleTotal;
std::chrono::milliseconds g_pool_idle_timeout{kDefaultPoolIdleTimeoutMs};
std::chrono::milliseconds g_pool_max_age{kDefaultPoolMaxAgeMs};
int g_upstream_connect_timeout_ms = kDefaultUpstreamConnectTimeoutMs;
int g_upstream_io_timeout_ms = kDefaultUpstreamIoTimeoutMs;
int g_client_io_timeout_ms = kDefaultClientIoTimeoutMs;
int g_max_active_clients = kDefaultMaxActiveClients;

bool try_acquire_client_slot() {
  int prev = g_active_clients.fetch_add(1, std::memory_order_acq_rel);
  if (prev >= g_max_active_clients) {
    g_active_clients.fetch_sub(1, std::memory_order_acq_rel);
    g_overload_rejections.fetch_add(1, std::memory_order_relaxed);
    return false;
  }
  return true;
}

void release_client_slot() {
  g_active_clients.fetch_sub(1, std::memory_order_acq_rel);
}

std::string trim(const std::string& s) {
  size_t begin = 0;
  while (begin < s.size() && std::isspace(static_cast<unsigned char>(s[begin]))) {
    begin++;
  }
  size_t end = s.size();
  while (end > begin && std::isspace(static_cast<unsigned char>(s[end - 1]))) {
    end--;
  }
  return s.substr(begin, end - begin);
}

std::string to_lower(std::string s) {
  std::transform(s.begin(), s.end(), s.begin(), [](unsigned char ch) {
    return static_cast<char>(std::tolower(ch));
  });
  return s;
}

std::string strip_port_from_host(const std::string& host) {
  auto pos = host.find(':');
  if (pos == std::string::npos) {
    return host;
  }
  return host.substr(0, pos);
}

bool parse_host_port(const std::string& target, std::string& host, int& port) {
  auto pos = target.rfind(':');
  if (pos == std::string::npos || pos == 0 || pos + 1 >= target.size()) {
    return false;
  }
  host = target.substr(0, pos);
  try {
    port = std::stoi(target.substr(pos + 1));
  } catch (...) {
    return false;
  }
  return port >= 1 && port <= 65535;
}

bool send_all(int fd, const char* data, size_t len) {
  size_t off = 0;
  while (off < len) {
    ssize_t n = ::send(fd, data + off, len - off, 0);
    if (n <= 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    off += static_cast<size_t>(n);
  }
  return true;
}

bool send_all(int fd, const std::string& data) {
  return send_all(fd, data.data(), data.size());
}

void send_simple_response(int fd, int status, const std::string& reason, const std::string& body) {
  std::ostringstream out;
  out << "HTTP/1.1 " << status << " " << reason << "\r\n"
      << kHeaderName << ": " << kHeaderValue << "\r\n"
      << "Content-Type: text/plain; charset=utf-8\r\n"
      << "Content-Length: " << body.size() << "\r\n"
      << "Connection: close\r\n\r\n"
      << body;
  (void)send_all(fd, out.str());
}

struct Request {
  std::string method;
  std::string path;
  std::string version;
  std::vector<std::pair<std::string, std::string>> headers;
  std::unordered_map<std::string, std::string> headers_lc;
  std::string body;
  std::string leftover;
  std::string normalized_host;
  bool chunked = false;
  bool client_wants_keepalive = false;
};

bool iequals_ascii(std::string_view a, std::string_view b) {
  if (a.size() != b.size()) {
    return false;
  }
  for (size_t i = 0; i < a.size(); ++i) {
    const unsigned char ac = static_cast<unsigned char>(a[i]);
    const unsigned char bc = static_cast<unsigned char>(b[i]);
    if (std::tolower(ac) != std::tolower(bc)) {
      return false;
    }
  }
  return true;
}

bool should_skip_forward_header(std::string_view key) {
  return iequals_ascii(key, "host") || iequals_ascii(key, "connection") ||
         iequals_ascii(key, "proxy-connection") || iequals_ascii(key, "x-forwarded-for") ||
         iequals_ascii(key, "x-forwarded-host") || iequals_ascii(key, "x-forwarded-proto") ||
         iequals_ascii(key, "content-length") || iequals_ascii(key, "transfer-encoding");
}

bool request_wants_keepalive(const Request& req) {
  bool connection_close = false;
  bool connection_keepalive = false;
  if (auto it = req.headers_lc.find("connection"); it != req.headers_lc.end()) {
    const std::string connection = to_lower(it->second);
    connection_close = connection.find("close") != std::string::npos;
    connection_keepalive = connection.find("keep-alive") != std::string::npos;
  }

  const std::string version = to_lower(req.version);
  if (version == "http/1.1") {
    return !connection_close;
  }
  if (version == "http/1.0") {
    return connection_keepalive;
  }
  return false;
}

bool recv_append(int fd, std::string& buf, std::string& error) {
  char tmp[kIoBufferSize];
  while (true) {
    ssize_t n = ::recv(fd, tmp, sizeof(tmp), 0);
    if (n == 0) {
      error = "client closed connection";
      return false;
    }
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      error = std::string("recv failed: ") + std::strerror(errno);
      return false;
    }
    buf.append(tmp, static_cast<size_t>(n));
    return true;
  }
}

bool ensure_bytes_available(int fd, std::string& buf, size_t need, std::string& error) {
  while (buf.size() < need) {
    if (!recv_append(fd, buf, error)) {
      return false;
    }
  }
  return true;
}

bool decode_chunked_body(int fd, std::string initial, std::string& out_body, std::string& leftover,
                         std::string& error) {
  out_body.clear();
  size_t cursor = 0;
  std::string buf = std::move(initial);

  for (;;) {
    while (true) {
      auto line_end = buf.find("\r\n", cursor);
      if (line_end != std::string::npos) {
        const std::string line = trim(buf.substr(cursor, line_end - cursor));
        cursor = line_end + 2;

        const auto semi = line.find(';');
        const std::string size_str = semi == std::string::npos ? line : line.substr(0, semi);
        size_t chunk_size = 0;
        try {
          chunk_size = static_cast<size_t>(std::stoull(size_str, nullptr, 16));
        } catch (...) {
          error = "invalid chunk size";
          return false;
        }

        if (!ensure_bytes_available(fd, buf, cursor + chunk_size + 2, error)) {
          return false;
        }

        if (chunk_size == 0) {
          // Consume trailer headers until empty line.
          for (;;) {
            auto trailer_end = buf.find("\r\n", cursor);
            while (trailer_end == std::string::npos) {
              if (!recv_append(fd, buf, error)) {
                return false;
              }
              trailer_end = buf.find("\r\n", cursor);
            }
            const std::string trailer_line = buf.substr(cursor, trailer_end - cursor);
            cursor = trailer_end + 2;
            if (trailer_line.empty()) {
              leftover = buf.substr(cursor);
              return true;
            }
          }
        }

        out_body.append(buf, cursor, chunk_size);
        cursor += chunk_size;
        if (buf.substr(cursor, 2) != "\r\n") {
          error = "invalid chunk terminator";
          return false;
        }
        cursor += 2;

        break;
      }
      if (!recv_append(fd, buf, error)) {
        return false;
      }
    }
  }
}

bool read_request(int client_fd, std::string& pending, Request& req, std::string& error) {
  req = Request{};
  std::string buf = std::move(pending);
  pending.clear();
  if (buf.capacity() < 8192) {
    buf.reserve(8192);
  }

  char tmp[kIoBufferSize];
  size_t header_end = std::string::npos;
  while (true) {
    header_end = buf.find("\r\n\r\n");
    if (header_end != std::string::npos) {
      break;
    }
    if (buf.size() > kMaxHeaderBytes) {
      error = "request headers too large";
      return false;
    }

    ssize_t n = ::recv(client_fd, tmp, sizeof(tmp), 0);
    if (n == 0) {
      error = "client closed before request";
      return false;
    }
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      error = std::string("recv failed: ") + std::strerror(errno);
      return false;
    }
    buf.append(tmp, static_cast<size_t>(n));
  }

  const size_t headers_len = header_end + 4;
  std::string headers_blob = buf.substr(0, headers_len);

  std::istringstream header_stream(headers_blob);
  std::string line;

  if (!std::getline(header_stream, line)) {
    error = "missing request line";
    return false;
  }
  if (!line.empty() && line.back() == '\r') {
    line.pop_back();
  }

  {
    std::istringstream rl(line);
    if (!(rl >> req.method >> req.path >> req.version)) {
      error = "invalid request line";
      return false;
    }
  }

  while (std::getline(header_stream, line)) {
    if (!line.empty() && line.back() == '\r') {
      line.pop_back();
    }
    if (line.empty()) {
      break;
    }
    auto pos = line.find(':');
    if (pos == std::string::npos) {
      continue;
    }
    std::string key = trim(line.substr(0, pos));
    std::string val = trim(line.substr(pos + 1));
    req.headers.emplace_back(key, val);
    req.headers_lc[to_lower(key)] = val;
  }

  if (auto host_it = req.headers_lc.find("host"); host_it != req.headers_lc.end()) {
    req.normalized_host = to_lower(strip_port_from_host(trim(host_it->second)));
  }

  bool chunked = false;
  size_t content_length = 0;
  if (auto it = req.headers_lc.find("content-length"); it != req.headers_lc.end()) {
    try {
      content_length = static_cast<size_t>(std::stoul(it->second));
    } catch (...) {
      error = "invalid content-length";
      return false;
    }
  }

  if (auto it = req.headers_lc.find("transfer-encoding"); it != req.headers_lc.end()) {
    if (to_lower(it->second).find("chunked") != std::string::npos) {
      chunked = true;
    }
  }
  req.chunked = chunked;

  std::string initial = buf.substr(headers_len);
  if (chunked) {
    const bool ok = decode_chunked_body(client_fd, std::move(initial), req.body, req.leftover, error);
    if (ok) {
      req.client_wants_keepalive = request_wants_keepalive(req);
      pending = req.leftover;
    }
    return ok;
  }

  if (initial.size() >= content_length) {
    req.body = initial.substr(0, content_length);
    req.leftover = initial.substr(content_length);
    req.client_wants_keepalive = request_wants_keepalive(req);
    pending = req.leftover;
    return true;
  }

  req.body = std::move(initial);
  req.body.reserve(content_length);
  while (req.body.size() < content_length) {
    ssize_t n = ::recv(client_fd, tmp, sizeof(tmp), 0);
    if (n <= 0) {
      if (n < 0 && errno == EINTR) {
        continue;
      }
      error = "client closed before full request body";
      return false;
    }
    req.body.append(tmp, static_cast<size_t>(n));
  }
  if (req.body.size() > content_length) {
    req.leftover = req.body.substr(content_length);
    req.body.resize(content_length);
  }
  req.client_wants_keepalive = request_wants_keepalive(req);
  pending = req.leftover;
  return true;
}

class RouteTable {
 public:
  explicit RouteTable(std::string routes_path) : routes_path_(std::move(routes_path)) {}

  std::optional<std::string> lookup(const std::string& host) {
    reload_if_needed();
    std::lock_guard<std::mutex> lock(mu_);
    auto it = routes_.find(to_lower(host));
    if (it == routes_.end()) {
      return std::nullopt;
    }
    return it->second;
  }

  size_t size() {
    reload_if_needed();
    std::lock_guard<std::mutex> lock(mu_);
    return routes_.size();
  }

 private:
  void reload_if_needed() {
    const auto now = std::chrono::steady_clock::now();
    {
      std::lock_guard<std::mutex> lock(mu_);
      if (loaded_ &&
          now - last_reload_check_ <
              std::chrono::milliseconds(kDefaultRouteReloadCheckIntervalMs)) {
        return;
      }
      last_reload_check_ = now;
    }

    std::error_code ec;
    auto current = std::filesystem::last_write_time(routes_path_, ec);
    if (ec) {
      return;
    }

    {
      std::lock_guard<std::mutex> lock(mu_);
      if (loaded_ && current == mtime_) {
        return;
      }
    }

    std::ifstream in(routes_path_);
    if (!in) {
      return;
    }

    std::ostringstream raw;
    raw << in.rdbuf();

    std::unordered_map<std::string, std::string> parsed;
    static const std::regex pair_re("\\\"([^\\\"]+)\\\"\\s*:\\s*\\\"([^\\\"]*)\\\"");

    const std::string content = raw.str();
    auto begin = std::sregex_iterator(content.begin(), content.end(), pair_re);
    auto end = std::sregex_iterator();
    for (auto it = begin; it != end; ++it) {
      const std::string host = to_lower((*it)[1].str());
      const std::string target = trim((*it)[2].str());
      if (!host.empty() && !target.empty()) {
        parsed[host] = target;
      }
    }

    std::lock_guard<std::mutex> lock(mu_);
    routes_ = std::move(parsed);
    mtime_ = current;
    loaded_ = true;
  }

  std::string routes_path_;
  std::unordered_map<std::string, std::string> routes_;
  std::filesystem::file_time_type mtime_{};
  std::chrono::steady_clock::time_point last_reload_check_{};
  bool loaded_ = false;
  std::mutex mu_;
};

void set_common_socket_opts(int fd) {
  int one = 1;
  (void)setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
  (void)setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof(one));
}

void set_socket_timeouts_ms(int fd, int timeout_ms) {
  timeval tv{};
  tv.tv_sec = timeout_ms / 1000;
  tv.tv_usec = (timeout_ms % 1000) * 1000;
  (void)setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
  (void)setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
}

bool set_nonblocking(int fd, bool nonblocking) {
  int flags = fcntl(fd, F_GETFL, 0);
  if (flags < 0) {
    return false;
  }
  if (nonblocking) {
    flags |= O_NONBLOCK;
  } else {
    flags &= ~O_NONBLOCK;
  }
  return fcntl(fd, F_SETFL, flags) == 0;
}

bool connect_with_timeout(int fd, const sockaddr* addr, socklen_t addrlen, int timeout_ms) {
  if (!set_nonblocking(fd, true)) {
    return false;
  }
  int rc = connect(fd, addr, addrlen);
  if (rc == 0) {
    (void)set_nonblocking(fd, false);
    return true;
  }
  if (errno != EINPROGRESS) {
    return false;
  }

  pollfd pfd{};
  pfd.fd = fd;
  pfd.events = POLLOUT;
  while (true) {
    int prc = poll(&pfd, 1, timeout_ms);
    if (prc == 0) {
      errno = ETIMEDOUT;
      return false;
    }
    if (prc < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    int so_error = 0;
    socklen_t slen = sizeof(so_error);
    if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &so_error, &slen) < 0) {
      return false;
    }
    if (so_error != 0) {
      errno = so_error;
      return false;
    }
    (void)set_nonblocking(fd, false);
    return true;
  }
}

int connect_upstream(const std::string& host, int port) {
  struct addrinfo hints;
  std::memset(&hints, 0, sizeof(hints));
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = SOCK_STREAM;

  struct addrinfo* res = nullptr;
  const std::string port_str = std::to_string(port);
  int rc = getaddrinfo(host.c_str(), port_str.c_str(), &hints, &res);
  if (rc != 0) {
    return -1;
  }

  int fd = -1;
  for (auto* p = res; p != nullptr; p = p->ai_next) {
    fd = socket(p->ai_family, p->ai_socktype, p->ai_protocol);
    if (fd < 0) {
      continue;
    }
    set_common_socket_opts(fd);
    if (connect_with_timeout(fd, p->ai_addr, p->ai_addrlen, g_upstream_connect_timeout_ms)) {
      set_socket_timeouts_ms(fd, g_upstream_io_timeout_ms);
      break;
    }
    close(fd);
    fd = -1;
  }

  freeaddrinfo(res);
  return fd;
}

bool socket_is_idle_usable(int fd) {
  char c;
  ssize_t n = recv(fd, &c, 1, MSG_PEEK | MSG_DONTWAIT);
  if (n == 0) {
    return false;
  }
  if (n < 0) {
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
      return true;
    }
    if (errno == EINTR) {
      return socket_is_idle_usable(fd);
    }
    return false;
  }
  // Data pending means stream is not in a clean idle state for reuse.
  return false;
}

struct PooledConn {
  int fd = -1;
  std::chrono::steady_clock::time_point created_at{};
  std::chrono::steady_clock::time_point last_used_at{};
};

class UpstreamPool {
 public:
  ~UpstreamPool() {
    std::lock_guard<std::mutex> lock(mu_);
    for (auto& [_, conns] : by_key_) {
      for (auto& conn : conns) {
        if (conn.fd >= 0) {
          close(conn.fd);
        }
      }
    }
  }

  int acquire(const std::string& key, const std::string& host, int port) {
    const auto now = std::chrono::steady_clock::now();
    {
      std::lock_guard<std::mutex> lock(mu_);
      reap_locked(now);
      auto it = by_key_.find(key);
      if (it != by_key_.end()) {
        auto& conns = it->second;
        while (!conns.empty()) {
          auto conn = conns.back();
          conns.pop_back();
          idle_total_ = idle_total_ > 0 ? idle_total_ - 1 : 0;
          if (!is_conn_fresh(now, conn) || !socket_is_idle_usable(conn.fd)) {
            close(conn.fd);
            continue;
          }
          return conn.fd;
        }
      }
    }
    return connect_upstream(host, port);
  }

  void release(const std::string& key, int fd) {
    if (fd < 0) {
      return;
    }
    if (!socket_is_idle_usable(fd)) {
      close(fd);
      return;
    }

    const auto now = std::chrono::steady_clock::now();
    std::lock_guard<std::mutex> lock(mu_);
    reap_locked(now);
    if (idle_total_ >= g_pool_max_idle_total) {
      close(fd);
      return;
    }
    auto& conns = by_key_[key];
    if (conns.size() >= g_pool_max_idle_per_key) {
      close(fd);
      return;
    }
    conns.push_back(PooledConn{
        .fd = fd,
        .created_at = now,
        .last_used_at = now,
    });
    idle_total_++;
  }

  void discard(int fd) {
    if (fd >= 0) {
      close(fd);
    }
  }

 private:
  bool is_conn_fresh(const std::chrono::steady_clock::time_point& now, const PooledConn& conn) {
    if (now - conn.last_used_at > g_pool_idle_timeout) {
      return false;
    }
    if (now - conn.created_at > g_pool_max_age) {
      return false;
    }
    return true;
  }

  void reap_locked(const std::chrono::steady_clock::time_point& now) {
    for (auto it = by_key_.begin(); it != by_key_.end();) {
      auto& conns = it->second;
      size_t write = 0;
      for (size_t read = 0; read < conns.size(); ++read) {
        if (!is_conn_fresh(now, conns[read]) || !socket_is_idle_usable(conns[read].fd)) {
          close(conns[read].fd);
          idle_total_ = idle_total_ > 0 ? idle_total_ - 1 : 0;
          continue;
        }
        if (write != read) {
          conns[write] = conns[read];
        }
        write++;
      }
      conns.resize(write);
      if (conns.empty()) {
        it = by_key_.erase(it);
      } else {
        ++it;
      }
    }
  }

  std::mutex mu_;
  std::unordered_map<std::string, std::vector<PooledConn>> by_key_;
  size_t idle_total_ = 0;
};

UpstreamPool g_upstream_pool;

bool is_upgrade_request(const Request& req) {
  auto upgrade_it = req.headers_lc.find("upgrade");
  if (upgrade_it == req.headers_lc.end()) {
    return false;
  }
  auto conn_it = req.headers_lc.find("connection");
  if (conn_it == req.headers_lc.end()) {
    return false;
  }
  return to_lower(conn_it->second).find("upgrade") != std::string::npos;
}

std::string build_upstream_request(const Request& req, const std::string& host_header,
                                   bool tunnel_upgrade, bool keepalive_upstream) {
  std::string out;
  out.reserve(512 + req.method.size() + req.path.size() + req.version.size() + req.body.size());
  out.append(req.method).append(" ").append(req.path).append(" ").append(req.version).append("\r\n");

  for (const auto& [key, value] : req.headers) {
    if (should_skip_forward_header(key)) {
      continue;
    }
    out.append(key).append(": ").append(value).append("\r\n");
  }

  out.append("Host: ").append(host_header).append("\r\n");
  auto host_it = req.headers_lc.find("host");
  std::string original_host = host_it == req.headers_lc.end() ? host_header : host_it->second;
  out.append("X-Forwarded-Host: ").append(original_host).append("\r\n");
  out.append("X-Forwarded-Proto: http\r\n");
  if (tunnel_upgrade) {
    auto up_it = req.headers_lc.find("upgrade");
    std::string up = up_it == req.headers_lc.end() ? "websocket" : up_it->second;
    out.append("Connection: Upgrade\r\n");
    out.append("Upgrade: ").append(up).append("\r\n");
    out.append("\r\n");
  } else {
    out.append("Connection: ").append(keepalive_upstream ? "keep-alive" : "close").append("\r\n");
    out.append("Content-Length: ").append(std::to_string(req.body.size())).append("\r\n\r\n");
    out.append(req.body);
  }
  return out;
}

void shutdown_quiet(int fd, int how) {
  if (fd >= 0) {
    (void)shutdown(fd, how);
  }
}

void pump_fd(int src, int dst, std::atomic<bool>& done) {
  char buf[kIoBufferSize];
  while (!done.load()) {
    ssize_t n = recv(src, buf, sizeof(buf), 0);
    if (n == 0) {
      break;
    }
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      break;
    }
    if (!send_all(dst, buf, static_cast<size_t>(n))) {
      break;
    }
  }
  done.store(true);
  shutdown_quiet(dst, SHUT_WR);
  shutdown_quiet(src, SHUT_RD);
}

void tunnel_bidirectional(int a_fd, int b_fd) {
  std::atomic<bool> done{false};
  std::thread upstream_to_client([&]() { pump_fd(b_fd, a_fd, done); });
  pump_fd(a_fd, b_fd, done);
  upstream_to_client.join();
}

struct ResponseMeta {
  int status_code = 0;
  bool chunked = false;
  bool connection_close = false;
  bool no_body = false;
  std::optional<size_t> content_length;
};

bool parse_response_headers(const std::string& raw_headers, const std::string& req_method, ResponseMeta& out) {
  std::istringstream s(raw_headers);
  std::string line;
  if (!std::getline(s, line)) {
    return false;
  }
  if (!line.empty() && line.back() == '\r') {
    line.pop_back();
  }
  {
    std::istringstream first(line);
    std::string http_version;
    if (!(first >> http_version >> out.status_code)) {
      return false;
    }
  }

  while (std::getline(s, line)) {
    if (!line.empty() && line.back() == '\r') {
      line.pop_back();
    }
    if (line.empty()) {
      break;
    }
    auto pos = line.find(':');
    if (pos == std::string::npos) {
      continue;
    }
    auto key = to_lower(trim(line.substr(0, pos)));
    auto val = to_lower(trim(line.substr(pos + 1)));
    if (key == "transfer-encoding" && val.find("chunked") != std::string::npos) {
      out.chunked = true;
    } else if (key == "content-length") {
      try {
        out.content_length = static_cast<size_t>(std::stoull(val));
      } catch (...) {
        return false;
      }
    } else if (key == "connection" && val.find("close") != std::string::npos) {
      out.connection_close = true;
    }
  }

  const std::string method = to_lower(req_method);
  const bool informational = out.status_code >= 100 && out.status_code < 200 && out.status_code != 101;
  out.no_body = (method == "head") || informational || out.status_code == 204 || out.status_code == 304;
  if (out.no_body) {
    out.chunked = false;
    out.content_length = 0;
  }
  return true;
}

bool recv_append_upstream(int fd, std::string& buf) {
  char tmp[kIoBufferSize];
  while (true) {
    ssize_t n = recv(fd, tmp, sizeof(tmp), 0);
    if (n == 0) {
      return false;
    }
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    buf.append(tmp, static_cast<size_t>(n));
    return true;
  }
}

bool relay_body_with_length(int upstream_fd, int client_fd, std::string body_buf, size_t body_len) {
  size_t sent = 0;
  if (!body_buf.empty()) {
    size_t first = std::min(body_buf.size(), body_len);
    if (first > 0 && !send_all(client_fd, body_buf.data(), first)) {
      return false;
    }
    sent += first;
    if (body_buf.size() > body_len) {
      return false;
    }
  }

  char tmp[kIoBufferSize];
  while (sent < body_len) {
    ssize_t n = recv(upstream_fd, tmp, sizeof(tmp), 0);
    if (n == 0) {
      return false;
    }
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    size_t to_send = std::min(static_cast<size_t>(n), body_len - sent);
    if (!send_all(client_fd, tmp, to_send)) {
      return false;
    }
    sent += to_send;
    if (static_cast<size_t>(n) > to_send) {
      // Unexpected bytes beyond declared content-length. Treat as non-reusable.
      return false;
    }
  }
  return true;
}

bool relay_chunked_body(int upstream_fd, int client_fd, std::string buf) {
  size_t cursor = 0;
  for (;;) {
    while (true) {
      auto line_end = buf.find("\r\n", cursor);
      if (line_end == std::string::npos) {
        if (!recv_append_upstream(upstream_fd, buf)) {
          return false;
        }
        continue;
      }

      const std::string line = trim(buf.substr(cursor, line_end - cursor));
      const auto semi = line.find(';');
      const std::string size_str = semi == std::string::npos ? line : line.substr(0, semi);
      size_t chunk_size = 0;
      try {
        chunk_size = static_cast<size_t>(std::stoull(size_str, nullptr, 16));
      } catch (...) {
        return false;
      }

      const size_t chunk_prefix = line_end + 2;
      while (buf.size() < chunk_prefix + chunk_size + 2) {
        if (!recv_append_upstream(upstream_fd, buf)) {
          return false;
        }
      }
      if (buf.substr(chunk_prefix + chunk_size, 2) != "\r\n") {
        return false;
      }

      if (!send_all(client_fd, buf.data() + cursor, chunk_prefix + chunk_size + 2 - cursor)) {
        return false;
      }
      cursor = chunk_prefix + chunk_size + 2;

      if (chunk_size == 0) {
        // Forward trailers and ending CRLF.
        auto trailer_end = buf.find("\r\n\r\n", cursor);
        while (trailer_end == std::string::npos) {
          if (!recv_append_upstream(upstream_fd, buf)) {
            return false;
          }
          trailer_end = buf.find("\r\n\r\n", cursor);
        }
        const size_t end = trailer_end + 4;
        if (!send_all(client_fd, buf.data() + cursor, end - cursor)) {
          return false;
        }
        return end == buf.size();
      }
      break;
    }
  }
}

struct RelayOutcome {
  bool upstream_reusable = false;
  bool client_can_keepalive = false;
};

RelayOutcome relay_response_and_decide_reuse(int upstream_fd, int client_fd, const std::string& req_method) {
  std::string buf;
  buf.reserve(8192);
  size_t header_end = std::string::npos;
  while (header_end == std::string::npos) {
    header_end = buf.find("\r\n\r\n");
    if (header_end != std::string::npos) {
      break;
    }
    if (!recv_append_upstream(upstream_fd, buf)) {
      return {};
    }
    if (buf.size() > kMaxHeaderBytes) {
      return {};
    }
  }

  const size_t hdr_len = header_end + 4;
  const std::string raw_headers = buf.substr(0, hdr_len);
  ResponseMeta meta;
  if (!parse_response_headers(raw_headers, req_method, meta)) {
    return {};
  }
  if (!send_all(client_fd, raw_headers)) {
    return {};
  }

  std::string body_buf = buf.substr(hdr_len);
  if (meta.no_body) {
    if (!body_buf.empty()) {
      if (!send_all(client_fd, body_buf)) {
        return {};
      }
      return {};
    }
    return {
        .upstream_reusable = !meta.connection_close,
        .client_can_keepalive = !meta.connection_close,
    };
  }

  if (meta.chunked) {
    bool complete = relay_chunked_body(upstream_fd, client_fd, std::move(body_buf));
    const bool keepalive = complete && !meta.connection_close;
    return {
        .upstream_reusable = keepalive,
        .client_can_keepalive = keepalive,
    };
  }

  if (meta.content_length.has_value()) {
    bool ok = relay_body_with_length(upstream_fd, client_fd, std::move(body_buf), *meta.content_length);
    const bool keepalive = ok && !meta.connection_close;
    return {
        .upstream_reusable = keepalive,
        .client_can_keepalive = keepalive,
    };
  }

  // Unknown body framing: read until close and do not reuse socket.
  if (!body_buf.empty() && !send_all(client_fd, body_buf)) {
    return {};
  }
  char tmp[kIoBufferSize];
  while (true) {
    ssize_t n = recv(upstream_fd, tmp, sizeof(tmp), 0);
    if (n == 0) {
      break;
    }
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      return {};
    }
    if (!send_all(client_fd, tmp, static_cast<size_t>(n))) {
      return {};
    }
  }
  return {};
}

void handle_client(int client_fd, RouteTable& routes) {
  std::string pending;
  int cached_upstream_fd = -1;
  std::string cached_upstream_key;
  auto discard_cached = [&]() {
    if (cached_upstream_fd >= 0) {
      g_upstream_pool.discard(cached_upstream_fd);
      cached_upstream_fd = -1;
      cached_upstream_key.clear();
    }
  };
  auto release_cached = [&]() {
    if (cached_upstream_fd >= 0 && !cached_upstream_key.empty()) {
      g_upstream_pool.release(cached_upstream_key, cached_upstream_fd);
      cached_upstream_fd = -1;
      cached_upstream_key.clear();
    }
  };

  while (g_running.load(std::memory_order_relaxed)) {
    Request req;
    std::string parse_error;
    if (!read_request(client_fd, pending, req, parse_error)) {
      if (!parse_error.empty() && parse_error != "client closed before request" &&
          parse_error != "client closed connection") {
        send_simple_response(client_fd, 400, "Bad Request", parse_error + "\n");
      }
      break;
    }

    if (req.path == "/_flow/domains/health") {
      std::ostringstream body;
      body << "ok active_clients=" << g_active_clients.load(std::memory_order_relaxed)
           << " overload_rejections=" << g_overload_rejections.load(std::memory_order_relaxed)
           << " max_active_clients=" << g_max_active_clients
           << " upstream_connect_timeout_ms=" << g_upstream_connect_timeout_ms
           << " upstream_io_timeout_ms=" << g_upstream_io_timeout_ms
           << " client_io_timeout_ms=" << g_client_io_timeout_ms
           << " pool_max_idle_per_key=" << g_pool_max_idle_per_key
           << " pool_max_idle_total=" << g_pool_max_idle_total
           << " pool_idle_timeout_ms=" << g_pool_idle_timeout.count()
           << " pool_max_age_ms=" << g_pool_max_age.count()
           << "\n";
      const auto body_s = body.str();
      std::ostringstream out;
      out << "HTTP/1.1 200 OK\r\n"
          << kHeaderName << ": " << kHeaderValue << "\r\n"
          << "Content-Type: text/plain; charset=utf-8\r\n"
          << "Content-Length: " << body_s.size() << "\r\n"
          << "Connection: " << (req.client_wants_keepalive ? "keep-alive" : "close")
          << "\r\n\r\n"
          << body_s;
      if (!send_all(client_fd, out.str()) || !req.client_wants_keepalive) {
        break;
      }
      continue;
    }

    if (req.normalized_host.empty()) {
      send_simple_response(client_fd, 400, "Bad Request", "Missing Host header\n");
      break;
    }

    const std::string& req_host = req.normalized_host;
    auto target = routes.lookup(req_host);
    if (!target.has_value()) {
      std::ostringstream body;
      body << "No local route configured for " << req_host << "\n";
      send_simple_response(client_fd, 404, "Not Found", body.str());
      break;
    }

    std::string upstream_host;
    int upstream_port = 0;
    if (!parse_host_port(*target, upstream_host, upstream_port)) {
      send_simple_response(client_fd, 502, "Bad Gateway", "Invalid target route\n");
      break;
    }

    const bool upgrade = is_upgrade_request(req);
    const std::string upstream_key = upstream_host + ":" + std::to_string(upstream_port);
    if (upgrade) {
      // Upgrade tunnels are one-shot; keepalive cache is irrelevant.
      release_cached();
    }

    bool used_cached = false;
    int upstream_fd = -1;
    if (!upgrade && cached_upstream_fd >= 0 && cached_upstream_key == upstream_key) {
      upstream_fd = cached_upstream_fd;
      used_cached = true;
    } else {
      if (!upgrade) {
        release_cached();
      }
      upstream_fd =
          upgrade ? connect_upstream(upstream_host, upstream_port)
                  : g_upstream_pool.acquire(upstream_key, upstream_host, upstream_port);
    }

    if (upstream_fd < 0) {
      if (errno == ETIMEDOUT) {
        send_simple_response(client_fd, 504, "Gateway Timeout", "Upstream connect timed out\n");
      } else {
        send_simple_response(client_fd, 502, "Bad Gateway", "Upstream connection failed\n");
      }
      break;
    }

    std::string host_header =
        (upstream_host == "127.0.0.1" || upstream_host == "::1") ? "localhost" : upstream_host;
    std::string upstream_req = build_upstream_request(req, host_header, upgrade, true);
    if (!send_all(upstream_fd, upstream_req)) {
      // Stale keepalive sockets can fail first write; retry once with fresh socket.
      if (!upgrade && used_cached) {
        discard_cached();
        upstream_fd = g_upstream_pool.acquire(upstream_key, upstream_host, upstream_port);
        if (upstream_fd >= 0 && send_all(upstream_fd, upstream_req)) {
          used_cached = false;
        } else if (upstream_fd >= 0) {
          g_upstream_pool.discard(upstream_fd);
          upstream_fd = -1;
        }
      } else if (upgrade) {
        close(upstream_fd);
        upstream_fd = -1;
      } else {
        g_upstream_pool.discard(upstream_fd);
        upstream_fd = -1;
      }

      if (upstream_fd < 0) {
        send_simple_response(client_fd, 502, "Bad Gateway", "Failed to forward request\n");
        break;
      }
    }

    if (upgrade) {
      if (!req.leftover.empty() && !send_all(upstream_fd, req.leftover)) {
        close(upstream_fd);
        break;
      }
      tunnel_bidirectional(client_fd, upstream_fd);
      close(upstream_fd);
      break;
    }

    RelayOutcome relay = relay_response_and_decide_reuse(upstream_fd, client_fd, req.method);
    if (relay.upstream_reusable) {
      cached_upstream_fd = upstream_fd;
      cached_upstream_key = upstream_key;
    } else {
      g_upstream_pool.discard(upstream_fd);
      if (used_cached) {
        cached_upstream_fd = -1;
        cached_upstream_key.clear();
      }
    }

    if (!(req.client_wants_keepalive && relay.client_can_keepalive)) {
      break;
    }
  }
  release_cached();
  close(client_fd);
}

bool parse_listen(const std::string& listen, std::string& host, int& port) {
  return parse_host_port(listen, host, port);
}

bool parse_u64_arg(const std::string& raw, uint64_t& out) {
  if (raw.empty()) {
    return false;
  }
  size_t idx = 0;
  try {
    out = std::stoull(raw, &idx, 10);
  } catch (...) {
    return false;
  }
  return idx == raw.size();
}

bool assign_positive_int(const std::string& value, int& target) {
  uint64_t parsed = 0;
  if (!parse_u64_arg(value, parsed) || parsed == 0 || parsed > static_cast<uint64_t>(INT32_MAX)) {
    return false;
  }
  target = static_cast<int>(parsed);
  return true;
}

bool assign_positive_size(const std::string& value, size_t& target) {
  uint64_t parsed = 0;
  if (!parse_u64_arg(value, parsed) || parsed == 0 ||
      parsed > static_cast<uint64_t>(std::numeric_limits<size_t>::max())) {
    return false;
  }
  target = static_cast<size_t>(parsed);
  return true;
}

void cleanup_pidfile() {
  if (!g_pidfile.empty()) {
    std::error_code ec;
    std::filesystem::remove(g_pidfile, ec);
  }
}

void on_signal(int) {
  g_running.store(false);
  if (g_listen_fd >= 0) {
    close(g_listen_fd);
    g_listen_fd = -1;
  }
}

int start_listener(const std::string& host, int port) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    return -1;
  }

  int opt = 1;
  if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt)) < 0) {
    close(fd);
    return -1;
  }

  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_port = htons(static_cast<uint16_t>(port));
  if (inet_pton(AF_INET, host.c_str(), &addr.sin_addr) != 1) {
    close(fd);
    return -1;
  }

  if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
    close(fd);
    return -1;
  }

  if (listen(fd, 256) < 0) {
    close(fd);
    return -1;
  }

  return fd;
}

int start_listener_from_launchd_socket(const std::string& socket_name) {
#ifdef __APPLE__
  int* fds = nullptr;
  size_t count = 0;
  const int rc = launch_activate_socket(socket_name.c_str(), &fds, &count);
  if (rc != 0) {
    errno = rc;
    return -1;
  }
  if (count == 0 || fds == nullptr) {
    errno = ENOENT;
    return -1;
  }
  int fd = fds[0];
  for (size_t i = 1; i < count; ++i) {
    if (fds[i] >= 0) {
      close(fds[i]);
    }
  }
  std::free(fds);
  return fd;
#else
  (void)socket_name;
  errno = ENOTSUP;
  return -1;
#endif
}

void print_usage(const char* argv0) {
  std::cerr << "Usage: " << argv0
            << " --listen 127.0.0.1:80 --routes <routes.json> --pidfile <domainsd.pid> [options]\n"
            << "Options:\n"
            << "  --launchd-socket <name> (macOS only)\n"
            << "  --max-active-clients <n>\n"
            << "  --upstream-connect-timeout-ms <ms>\n"
            << "  --upstream-io-timeout-ms <ms>\n"
            << "  --client-io-timeout-ms <ms>\n"
            << "  --pool-max-idle-per-key <n>\n"
            << "  --pool-max-idle-total <n>\n"
            << "  --pool-idle-timeout-ms <ms>\n"
            << "  --pool-max-age-ms <ms>\n";
}

}  // namespace

int main(int argc, char** argv) {
  std::string listen = "127.0.0.1:80";
  std::string routes_path;
  std::string pidfile;
  std::string launchd_socket_name;

  for (int i = 1; i < argc; ++i) {
    std::string arg = argv[i];
    if ((arg == "-h") || (arg == "--help")) {
      print_usage(argv[0]);
      return 0;
    }
    if (arg == "--listen" && i + 1 < argc) {
      listen = argv[++i];
      continue;
    }
    if (arg == "--routes" && i + 1 < argc) {
      routes_path = argv[++i];
      continue;
    }
    if (arg == "--pidfile" && i + 1 < argc) {
      pidfile = argv[++i];
      continue;
    }
    if (arg == "--launchd-socket" && i + 1 < argc) {
      launchd_socket_name = argv[++i];
      continue;
    }
    if (arg == "--max-active-clients" && i + 1 < argc) {
      if (!assign_positive_int(argv[++i], g_max_active_clients)) {
        std::cerr << "Invalid value for --max-active-clients\n";
        return 2;
      }
      continue;
    }
    if (arg == "--upstream-connect-timeout-ms" && i + 1 < argc) {
      if (!assign_positive_int(argv[++i], g_upstream_connect_timeout_ms)) {
        std::cerr << "Invalid value for --upstream-connect-timeout-ms\n";
        return 2;
      }
      continue;
    }
    if (arg == "--upstream-io-timeout-ms" && i + 1 < argc) {
      if (!assign_positive_int(argv[++i], g_upstream_io_timeout_ms)) {
        std::cerr << "Invalid value for --upstream-io-timeout-ms\n";
        return 2;
      }
      continue;
    }
    if (arg == "--client-io-timeout-ms" && i + 1 < argc) {
      if (!assign_positive_int(argv[++i], g_client_io_timeout_ms)) {
        std::cerr << "Invalid value for --client-io-timeout-ms\n";
        return 2;
      }
      continue;
    }
    if (arg == "--pool-max-idle-per-key" && i + 1 < argc) {
      if (!assign_positive_size(argv[++i], g_pool_max_idle_per_key)) {
        std::cerr << "Invalid value for --pool-max-idle-per-key\n";
        return 2;
      }
      continue;
    }
    if (arg == "--pool-max-idle-total" && i + 1 < argc) {
      if (!assign_positive_size(argv[++i], g_pool_max_idle_total)) {
        std::cerr << "Invalid value for --pool-max-idle-total\n";
        return 2;
      }
      continue;
    }
    if (arg == "--pool-idle-timeout-ms" && i + 1 < argc) {
      int ms = 0;
      if (!assign_positive_int(argv[++i], ms)) {
        std::cerr << "Invalid value for --pool-idle-timeout-ms\n";
        return 2;
      }
      g_pool_idle_timeout = std::chrono::milliseconds(ms);
      continue;
    }
    if (arg == "--pool-max-age-ms" && i + 1 < argc) {
      int ms = 0;
      if (!assign_positive_int(argv[++i], ms)) {
        std::cerr << "Invalid value for --pool-max-age-ms\n";
        return 2;
      }
      g_pool_max_age = std::chrono::milliseconds(ms);
      continue;
    }

    std::cerr << "Unknown or incomplete argument: " << arg << "\n";
    print_usage(argv[0]);
    return 2;
  }

  if (routes_path.empty() || pidfile.empty()) {
    print_usage(argv[0]);
    return 2;
  }

  std::string listen_host;
  int listen_port = 0;
  if (!parse_listen(listen, listen_host, listen_port)) {
    std::cerr << "Invalid --listen value: " << listen << "\n";
    return 2;
  }
  if (g_pool_max_idle_total < g_pool_max_idle_per_key) {
    g_pool_max_idle_total = g_pool_max_idle_per_key;
  }

  g_pidfile = pidfile;
  {
    std::ofstream out(pidfile, std::ios::trunc);
    if (!out) {
      std::cerr << "Failed to write pid file: " << pidfile << "\n";
      return 1;
    }
    out << getpid() << "\n";
  }

  std::signal(SIGINT, on_signal);
  std::signal(SIGTERM, on_signal);

  if (!launchd_socket_name.empty()) {
    g_listen_fd = start_listener_from_launchd_socket(launchd_socket_name);
  } else {
    g_listen_fd = start_listener(listen_host, listen_port);
  }
  if (g_listen_fd < 0) {
    cleanup_pidfile();
    if (!launchd_socket_name.empty()) {
      std::cerr << "Failed to activate launchd socket '" << launchd_socket_name << "' ("
                << std::strerror(errno) << ")\n";
    } else {
      std::cerr << "Failed to bind " << listen_host << ":" << listen_port << " ("
                << std::strerror(errno) << ")\n";
    }
    return 1;
  }

  if (!launchd_socket_name.empty()) {
    std::cerr << "domainsd-cpp listening via launchd socket '" << launchd_socket_name << "'\n";
  } else {
    std::cerr << "domainsd-cpp listening on " << listen_host << ":" << listen_port << "\n";
  }

  RouteTable routes(routes_path);
  while (g_running.load()) {
    sockaddr_in client_addr{};
    socklen_t client_len = sizeof(client_addr);
    int client_fd = accept(g_listen_fd, reinterpret_cast<sockaddr*>(&client_addr), &client_len);
    if (client_fd < 0) {
      if (errno == EINTR) {
        continue;
      }
      if (!g_running.load()) {
        break;
      }
      continue;
    }

    set_socket_timeouts_ms(client_fd, g_client_io_timeout_ms);
    if (!try_acquire_client_slot()) {
      send_simple_response(client_fd, 503, "Service Unavailable",
                           "Proxy overloaded, retry shortly\n");
      close(client_fd);
      continue;
    }

    std::thread([client_fd, &routes]() {
      struct SlotGuard {
        ~SlotGuard() { release_client_slot(); }
      } guard;
      handle_client(client_fd, routes);
    }).detach();
  }

  if (g_listen_fd >= 0) {
    close(g_listen_fd);
    g_listen_fd = -1;
  }
  cleanup_pidfile();
  return 0;
}
