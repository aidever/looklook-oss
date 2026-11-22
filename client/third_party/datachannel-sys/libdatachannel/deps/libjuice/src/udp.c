/**
 * Copyright (c) 2020 Paul-Louis Ageneau
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#include "udp.h"
#include "addr.h"
#include "log.h"
#include "random.h"
#include "thread.h" // for mutexes

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static struct addrinfo *find_family(struct addrinfo *ai_list, int family) {
	struct addrinfo *ai = ai_list;
	while (ai && ai->ai_family != family)
		ai = ai->ai_next;
	return ai;
}

static uint16_t get_next_port_in_range(uint16_t begin, uint16_t end) {
	if (begin == 0)
		begin = 1024;
	if (end == 0)
		end = 0xFFFF;
	if (begin == end)
		return begin;

	static volatile uint32_t count = 0;
	if (count == 0)
		count = juice_rand32();

	static mutex_t mutex = MUTEX_INITIALIZER;
	mutex_lock(&mutex);
	uint32_t diff = end > begin ? end - begin : 0;
	uint16_t next = begin + count++ % (diff + 1);
	mutex_unlock(&mutex);
	return next;
}

static socket_t create_socket_for_addrinfo(const udp_socket_config_t *config,
                                           const struct addrinfo *ai) {
	// Create socket
	socket_t sock = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
	if (sock == INVALID_SOCKET) {
		JLOG_WARN("UDP socket creation failed, errno=%d", sockerrno);
		return INVALID_SOCKET;
	}
	sock_no_inherit(sock); // looklook

	// Listen on both IPv6 and IPv4
	const sockopt_t disabled = 0;
	if (ai->ai_family == AF_INET6)
		setsockopt(sock, IPPROTO_IPV6, IPV6_V6ONLY, (const char *)&disabled, sizeof(disabled));

		// Set DF flag
#ifndef NO_PMTUDISC
	const sockopt_t val = IP_PMTUDISC_DO;
	setsockopt(sock, IPPROTO_IP, IP_MTU_DISCOVER, (const char *)&val, sizeof(val));
#ifdef IPV6_MTU_DISCOVER
	if (ai->ai_family == AF_INET6)
		setsockopt(sock, IPPROTO_IPV6, IPV6_MTU_DISCOVER, (const char *)&val, sizeof(val));
#endif
#else
	// It seems Mac OS lacks a way to set the DF flag...
	const sockopt_t enabled = 1;
#ifdef IP_DONTFRAG
	setsockopt(sock, IPPROTO_IP, IP_DONTFRAG, (const char *)&enabled, sizeof(enabled));
#endif
#ifdef IPV6_DONTFRAG
	if (ai->ai_family == AF_INET6)
		setsockopt(sock, IPPROTO_IPV6, IPV6_DONTFRAG, (const char *)&enabled, sizeof(enabled));
#endif
#endif

	// Set buffer size up to 1 MiB for performance
	const sockopt_t buffer_size = 1 * 1024 * 1024;
	setsockopt(sock, SOL_SOCKET, SO_RCVBUF, (const char *)&buffer_size, sizeof(buffer_size));
	setsockopt(sock, SOL_SOCKET, SO_SNDBUF, (const char *)&buffer_size, sizeof(buffer_size));

	ctl_t nbio = 1;
	if (ioctlsocket(sock, FIONBIO, &nbio)) {
		JLOG_ERROR("Setting non-blocking mode on UDP socket failed, errno=%d", sockerrno);
		goto error;
	}

	// Bind it
	if (config->port_begin == 0 && config->port_end == 0) {
		if (bind(sock, ai->ai_addr, (socklen_t)ai->ai_addrlen) == 0) {
			JLOG_DEBUG("UDP socket bound to %s:%hu",
			           config->bind_address ? config->bind_address : "any", udp_get_port(sock));
			return sock;
		}

		JLOG_WARN("UDP socket binding failed, errno=%d", sockerrno);

	} else if (config->port_begin == config->port_end) {
		uint16_t port = config->port_begin;
		struct sockaddr_storage addr;
		socklen_t addrlen = (socklen_t)ai->ai_addrlen;
		memcpy(&addr, ai->ai_addr, addrlen);
		addr_set_port((struct sockaddr *)&addr, port);

		if (bind(sock, (struct sockaddr *)&addr, addrlen) == 0) {
			JLOG_DEBUG("UDP socket bound to %s:%hu",
			           config->bind_address ? config->bind_address : "any", port);
			return sock;
		}

		JLOG_WARN("UDP socket binding failed on port %hu, errno=%d", port, sockerrno);

	} else {
		struct sockaddr_storage addr;
		socklen_t addrlen = (socklen_t)ai->ai_addrlen;
		memcpy(&addr, ai->ai_addr, addrlen);

		int retries = config->port_end - config->port_begin;
		do {
			uint16_t port = get_next_port_in_range(config->port_begin, config->port_end);
			addr_set_port((struct sockaddr *)&addr, port);
			if (bind(sock, (struct sockaddr *)&addr, addrlen) == 0) {
				JLOG_DEBUG("UDP socket bound to %s:%hu",
				           config->bind_address ? config->bind_address : "any", port);
				return sock;
			}
		} while ((sockerrno == SEADDRINUSE || sockerrno == SEACCES) && retries-- > 0);

		JLOG_WARN("UDP socket binding failed on port range %s:[%hu,%hu], errno=%d",
		          config->bind_address ? config->bind_address : "any", config->port_begin,
		          config->port_end, sockerrno);
	}

error:
	if (sock != INVALID_SOCKET)
		closesocket(sock);

	return INVALID_SOCKET;
}

socket_t udp_create_socket(const udp_socket_config_t *config) {
	// Obtain local Address
	struct addrinfo *ai_list = NULL;
	struct addrinfo hints;
	memset(&hints, 0, sizeof(hints));
	hints.ai_family = AF_UNSPEC;
	hints.ai_socktype = SOCK_DGRAM;
	hints.ai_protocol = IPPROTO_UDP;
	hints.ai_flags = AI_PASSIVE | AI_NUMERICSERV;
	if (getaddrinfo(config->bind_address, "0", &hints, &ai_list) != 0) {
		JLOG_ERROR("getaddrinfo for binding address failed, errno=%d", sockerrno);
		return INVALID_SOCKET;
	}

	const int families[2] = {AF_INET6, AF_INET}; // Prefer IPv6
	const char *names[2] = {"IPv6", "IPv4"};
	for (int i = 0; i < 2; ++i) {
		const struct addrinfo *ai = find_family(ai_list, families[i]);
		if (!ai)
			continue;

		JLOG_DEBUG("Opening UDP socket for %s family", names[i]);
		socket_t sock = create_socket_for_addrinfo(config, ai);
		if (sock != INVALID_SOCKET) {
			freeaddrinfo(ai_list);
			return sock;
		}
	}

	JLOG_ERROR("UDP socket opening failed");
	freeaddrinfo(ai_list);
	return INVALID_SOCKET;
}

int udp_recvfrom(socket_t sock, char *buffer, size_t size, addr_record_t *src) {
	while (true) {
		src->len = sizeof(src->addr);
		int len =
		    recvfrom(sock, buffer, (socklen_t)size, 0, (struct sockaddr *)&src->addr, &src->len);
		if (len >= 0) {
			addr_unmap_inet6_v4mapped((struct sockaddr *)&src->addr, &src->len);

		} else if (sockerrno == SECONNRESET || sockerrno == SENETRESET ||
		           sockerrno == SECONNREFUSED) {
			// On Windows, if a UDP socket receives an ICMP port unreachable response after
			// sending a datagram, this error is stored, and the next call to recvfrom() returns
			// WSAECONNRESET (port unreachable) or WSAENETRESET (TTL expired).
			// Therefore, it may be ignored.
			JLOG_DEBUG("Ignoring %s returned by recvfrom",
			           sockerrno == SECONNRESET
			               ? "ECONNRESET"
			               : (sockerrno == SENETRESET ? "ENETRESET" : "ECONNREFUSED"));
			continue;
		}
		return len;
	}
}

int udp_sendto(socket_t sock, const char *data, size_t size, const addr_record_t *dst) {
#ifndef __linux__
	addr_record_t tmp = *dst;
	addr_record_t name;
	name.len = sizeof(name.addr);
	if (getsockname(sock, (struct sockaddr *)&name.addr, &name.len) == 0) {
		if (name.addr.ss_family == AF_INET6)
			addr_map_inet6_v4mapped(&tmp.addr, &tmp.len);
	} else {
		JLOG_WARN("getsockname failed, errno=%d", sockerrno);
	}
	return sendto(sock, data, (socklen_t)size, 0, (const struct sockaddr *)&tmp.addr, tmp.len);
#else
	return sendto(sock, data, size, 0, (const struct sockaddr *)&dst->addr, dst->len);
#endif
}

int udp_sendto_self(socket_t sock, const char *data, size_t size) {
	addr_record_t local;
	if (udp_get_local_addr(sock, AF_UNSPEC, &local) < 0)
		return -1;

	int ret;
#ifndef __linux__
	// We know local has the same address family as sock here
	ret = sendto(sock, data, (socklen_t)size, 0, (const struct sockaddr *)&local.addr, local.len);
#else
	ret = sendto(sock, data, size, 0, (const struct sockaddr *)&local.addr, local.len);
#endif
	if (ret >= 0 || local.addr.ss_family != AF_INET6)
		return ret;

	// Fallback as IPv6 may be disabled on the loopback interface
	if (udp_get_local_addr(sock, AF_INET, &local) < 0)
		return -1;

#ifndef __linux__
	addr_map_inet6_v4mapped(&local.addr, &local.len);
	return sendto(sock, data, (socklen_t)size, 0, (const struct sockaddr *)&local.addr, local.len);
#else
	return sendto(sock, data, size, 0, (const struct sockaddr *)&local.addr, local.len);
#endif
}

int udp_set_diffserv(socket_t sock, int ds) {
#ifdef _WIN32
	// IP_TOS has been intentionally broken on Windows in favor of a convoluted proprietary
	// mechanism called qWave. Thank you Microsoft!
	// TODO: Investigate if DSCP can be still set directly without administrator flow configuration.
	(void)sock;
	(void)ds;
	JLOG_INFO("IP Differentiated Services are not supported on Windows");
	return -1;
#else
	addr_record_t name;
	name.len = sizeof(name.addr);
	if (getsockname(sock, (struct sockaddr *)&name.addr, &name.len) < 0) {
		JLOG_WARN("getsockname failed, errno=%d", sockerrno);
		return -1;
	}

	switch (name.addr.ss_family) {
	case AF_INET:
#ifdef IP_TOS
		if (setsockopt(sock, IPPROTO_IP, IP_TOS, &ds, sizeof(ds)) < 0) {
			JLOG_WARN("Setting IP ToS failed, errno=%d", sockerrno);
			return -1;
		}
		return 0;
#else
		JLOG_INFO("Setting IP ToS is not supported");
		return -1;
#endif

	case AF_INET6:
#ifdef IPV6_TCLASS
		if (setsockopt(sock, IPPROTO_IPV6, IPV6_TCLASS, &ds, sizeof(ds)) < 0) {
			JLOG_WARN("Setting IPv6 traffic class failed, errno=%d", sockerrno);
			return -1;
		}
#ifdef IP_TOS
		// Attempt to also set IP_TOS for IPv4, in case the system requires it
		setsockopt(sock, IPPROTO_IP, IP_TOS, &ds, sizeof(ds));
#endif
		return 0;
#else
		JLOG_INFO("Setting IPv6 traffic class is not supported");
		return -1;
#endif
	default:
		return -1;
	}
#endif
}

uint16_t udp_get_port(socket_t sock) {
	addr_record_t record;
	if (udp_get_bound_addr(sock, &record) < 0)
		return 0;
	return addr_get_port((struct sockaddr *)&record.addr);
}

int udp_get_bound_addr(socket_t sock, addr_record_t *record) {
	record->len = sizeof(record->addr);
	if (getsockname(sock, (struct sockaddr *)&record->addr, &record->len)) {
		JLOG_WARN("getsockname failed, errno=%d", sockerrno);
		return -1;
	}
	return 0;
}

int udp_get_local_addr(socket_t sock, int family_hint, addr_record_t *record) {
	if (udp_get_bound_addr(sock, record) < 0)
		return -1;

	// If the socket is bound to a particular address, return it
	if (!addr_is_any((struct sockaddr *)&record->addr)) {
		if (record->addr.ss_family == AF_INET && family_hint == AF_INET6)
			addr_map_inet6_v4mapped(&record->addr, &record->len);

		return 0;
	}

	if (record->addr.ss_family == AF_INET6 && family_hint == AF_INET) {
		// Generate an IPv4 instead (socket is listening to any IPv4 or IPv6)

		uint16_t port = addr_get_port((struct sockaddr *)&record->addr);
		if (port == 0)
			return -1;

		struct sockaddr_in *sin = (struct sockaddr_in *)&record->addr;
		memset(sin, 0, sizeof(*sin));
		sin->sin_family = AF_INET;
		sin->sin_port = htons(port);
		record->len = sizeof(*sin);
	}

	switch (record->addr.ss_family) {
	case AF_INET: {
		struct sockaddr_in *sin = (struct sockaddr_in *)&record->addr;
		const uint8_t localhost[4] = {127, 0, 0, 1};
		memcpy(&sin->sin_addr, localhost, 4);
		break;
	}
	case AF_INET6: {
		struct sockaddr_in6 *sin6 = (struct sockaddr_in6 *)&record->addr;
		uint8_t *b = (uint8_t *)&sin6->sin6_addr;
		memset(b, 0, 15);
		b[15] = 0x01; // localhost
		break;
	}
	default:
		// Ignore
		break;
	}

	if (record->addr.ss_family == AF_INET && family_hint == AF_INET6)
		addr_map_inet6_v4mapped(&record->addr, &record->len);

	return 0;
}

// looklook: host candidates are capped (MAX_HOST_CANDIDATES_COUNT, 9). On machines with Docker, VMs or a
// Clash/mihomo TUN, virtual interfaces used to take those slots and the real LAN address was dropped, so two
// computers on the same LAN never paired directly. Skip interfaces that never lead to another machine on the
// LAN, skip Clash fake-ip / TUN addresses (198.18.0.0/15), prefer the OS default-route IPv4 and interfaces
// with a gateway, and collect all candidates before sorting so truncation never drops LAN addresses.
static bool looklook_skip_ifname(const char *name) {
	static const char *prefixes[] = {"docker", "br-", "veth", "virbr", "vmnet", "vboxnet", "utun",   "tun",
	                                 "tap",    "Meta", "mihomo", "clash", "Clash", "bridge", "awdl", "llw",
	                                 "anpi",   "cni",  "flannel", "kube", "lxc",  "lxdbr",  "podman", "vmenet",
	                                 "feth",   "vnet", "cali", "cilium", "incusbr", "mpqemubr", "weave", "dummy",
	                                 "sit",    "gre",  "vxlan", "gif",   "stf",  "pktap",  "ipsec",  NULL};
	if (!name)
		return false;
	for (int i = 0; prefixes[i]; ++i)
		if (strncmp(name, prefixes[i], strlen(prefixes[i])) == 0)
			return true;
	return false;
}

static bool looklook_skip_addr(const struct sockaddr *sa) {
	if (sa->sa_family != AF_INET)
		return false;
	const uint8_t *b = (const uint8_t *)&((const struct sockaddr_in *)sa)->sin_addr;
	return b[0] == 198 && (b[1] == 18 || b[1] == 19);
}

#ifdef _WIN32
static bool looklook_wcontains_ci(const wchar_t *hay, const char *needle) {
	if (!hay || !needle || !*needle)
		return false;
	for (; *hay; ++hay) {
		const wchar_t *h = hay;
		const char *n = needle;
		while (*h && *n) {
			wchar_t hc = *h;
			unsigned char nc = (unsigned char)*n;
			if (hc >= L'A' && hc <= L'Z')
				hc += 32;
			if (nc >= 'A' && nc <= 'Z')
				nc += 32;
			if (hc != (wchar_t)nc)
				break;
			++h;
			++n;
		}
		if (!*n)
			return true;
	}
	return false;
}

static bool looklook_weq_ci(const wchar_t *a, const char *b) {
	if (!a || !b)
		return false;
	while (*a && *b) {
		wchar_t ac = *a;
		unsigned char bc = (unsigned char)*b;
		if (ac >= L'A' && ac <= L'Z')
			ac += 32;
		if (bc >= 'A' && bc <= 'Z')
			bc += 32;
		if (ac != (wchar_t)bc)
			return false;
		++a;
		++b;
	}
	return *a == 0 && *b == 0;
}

static bool looklook_skip_win_adapter(const wchar_t *friendly, const wchar_t *desc) {
	if (looklook_weq_ci(friendly, "Meta"))
		return true;
	static const char *patterns[] = {"vEthernet", "WSL",         "Hyper-V",  "VMware", "VirtualBox",
	                                 "VBox",      "Docker",      "Clash",    "mihomo", "sing-box",
	                                 "Wintun",    "TAP-Windows", "OpenVPN",  "Npcap",  "Loopback", NULL};
	for (int i = 0; patterns[i]; ++i) {
		if (looklook_wcontains_ci(friendly, patterns[i]) || looklook_wcontains_ci(desc, patterns[i]))
			return true;
	}
	return false;
}
#endif

static bool looklook_default_ipv4(uint32_t *out_addr) {
	socket_t s = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
	if (s == INVALID_SOCKET)
		return false;
#ifdef sock_no_inherit
	sock_no_inherit(s); // looklook
#endif
	struct sockaddr_in dst;
	memset(&dst, 0, sizeof(dst));
	dst.sin_family = AF_INET;
	dst.sin_port = htons(53);
	if (inet_pton(AF_INET, "1.1.1.1", &dst.sin_addr) != 1 ||
	    connect(s, (const struct sockaddr *)&dst, sizeof(dst)) != 0) {
		closesocket(s);
		return false;
	}
	struct sockaddr_in local;
	socklen_t len = sizeof(local);
	bool ok = getsockname(s, (struct sockaddr *)&local, &len) == 0 &&
	          local.sin_family == AF_INET &&
	          !addr_is_local((const struct sockaddr *)&local) &&
	          !looklook_skip_addr((const struct sockaddr *)&local);
	if (ok)
		memcpy(out_addr, &local.sin_addr, 4);
	closesocket(s);
	return ok;
}

// 0: default-route private IPv4, 1: other private IPv4 (RFC 1918), 2: default-route public IPv4,
// 3: other public IPv4, 4: global IPv6 (2000::/3), 5: CGNAT IPv4 (100.64/10), 6: ULA IPv6, 7: other.
// Combined with gateway presence (`tier * 2 + (has_gateway ? 0 : 1)`).
static int looklook_rank(const addr_record_t *r, uint32_t default_v4, bool has_default_v4, bool has_gateway) {
	int tier = 7;
	if (r->addr.ss_family == AF_INET6) {
		const uint8_t *b = (const uint8_t *)&((const struct sockaddr_in6 *)&r->addr)->sin6_addr;
		if ((b[0] & 0xE0) == 0x20)
			tier = 4;
		else if ((b[0] & 0xFE) == 0xFC)
			tier = 6;
		else
			tier = 7;
	} else if (r->addr.ss_family == AF_INET) {
		const struct sockaddr_in *sin = (const struct sockaddr_in *)&r->addr;
		const uint8_t *b = (const uint8_t *)&sin->sin_addr;
		bool is_default = has_default_v4 && memcmp(&sin->sin_addr, &default_v4, 4) == 0;
		if (b[0] == 10 || (b[0] == 172 && (b[1] & 0xF0) == 16) || (b[0] == 192 && b[1] == 168))
			tier = is_default ? 0 : 1;
		else if (b[0] == 100 && (b[1] & 0xC0) == 64)
			tier = 5;
		else
			tier = is_default ? 2 : 3;
	}
	return tier * 2 + (has_gateway ? 0 : 1);
}

static void looklook_sort(addr_record_t *records, int *ranks, size_t n) {
	// stable insertion sort, keeps the system's order within a rank
	for (size_t i = 1; i < n; ++i) {
		addr_record_t tmp = records[i];
		int r = ranks[i];
		size_t j = i;
		while (j > 0 && ranks[j - 1] > r) {
			records[j] = records[j - 1];
			ranks[j] = ranks[j - 1];
			--j;
		}
		records[j] = tmp;
		ranks[j] = r;
	}
}

// Helper function to check if a similar address already exists in records
// This function ignores the port
static int has_duplicate_addr(struct sockaddr *addr, const addr_record_t *records, size_t count) {
	for (size_t i = 0; i < count; ++i) {
		const addr_record_t *record = records + i;
		if (record->addr.ss_family == addr->sa_family) {
			switch (addr->sa_family) {
			case AF_INET: {
				// For IPv4, compare the whole address
				const struct sockaddr_in *rsin = (const struct sockaddr_in *)&record->addr;
				const struct sockaddr_in *asin = (const struct sockaddr_in *)addr;
				if (memcmp(&rsin->sin_addr, &asin->sin_addr, 4) == 0)
					return true;
				break;
			}
			case AF_INET6: {
				// For IPv6, compare the network part only
				const struct sockaddr_in6 *rsin6 = (const struct sockaddr_in6 *)&record->addr;
				const struct sockaddr_in6 *asin6 = (const struct sockaddr_in6 *)addr;
				if (memcmp(&rsin6->sin6_addr, &asin6->sin6_addr, 8) == 0) // compare first 64 bits
					return true;
				break;
			}
			}
		}
	}
	return false;
}

#if !defined(_WIN32) && defined(NO_IFADDRS)
// Helper function to get the IPv6 address of the default interface
static int get_local_default_inet6(uint16_t port, struct sockaddr_in6 *result) {
	const char *dummy_host = "2001:db8::1"; // dummy public unreachable address
	const uint16_t dummy_port = 9;          // discard port

	struct sockaddr_in6 sin6;
	memset(&sin6, 0, sizeof(sin6));
	sin6.sin6_family = AF_INET6;
	sin6.sin6_port = htons(dummy_port);
	if (inet_pton(AF_INET6, dummy_host, &sin6.sin6_addr) != 1)
		return -1;

	socket_t sock = socket(AF_INET6, SOCK_DGRAM, IPPROTO_UDP);
	if (sock == INVALID_SOCKET)
		return -1;
	sock_no_inherit(sock); // looklook

	if (connect(sock, (const struct sockaddr *)&sin6, sizeof(sin6)))
		goto error;

	socklen_t result_len = sizeof(*result);
	if (getsockname(sock, (struct sockaddr *)result, &result_len))
		goto error;

	if (result_len != sizeof(*result))
		goto error;

	addr_set_port((struct sockaddr *)result, port);
	closesocket(sock);
	return 0;

error:
	closesocket(sock);
	return -1;
}
#endif

int udp_get_addrs(socket_t sock, addr_record_t *records, size_t count) {
	addr_record_t bound;
	if (udp_get_bound_addr(sock, &bound) < 0) {
		JLOG_ERROR("Getting UDP bound address failed");
		return -1;
	}

	if (!addr_is_any((struct sockaddr *)&bound.addr)) {
		if (count > 0)
			records[0] = bound;

		return 1;
	}

	uint16_t port = addr_get_port((struct sockaddr *)&bound.addr);

	// RFC 8445 5.1.1.1. Host Candidates:
	// Addresses from a loopback interface MUST NOT be included in the candidate addresses.
	// [...]
	// If gathering one or more host candidates that correspond to an IPv6 address that was
	// generated using a mechanism that prevents location tracking [RFC7721], host candidates
	// that correspond to IPv6 addresses that do allow location tracking, are configured on the
	// same interface, and are part of the same network prefix MUST NOT be gathered. Similarly,
	// when host candidates corresponding to an IPv6 address generated using a mechanism that
	// prevents location tracking are gathered, then host candidates corresponding to IPv6
	// link-local addresses [RFC4291] MUST NOT be gathered. The IPv6 default address selection
	// specification [RFC6724] specifies that temporary addresses [RFC4941] are to be preferred
	// over permanent addresses.

	// IPv6 IIDs generated by modern systems are opaque so there is no way to reliably differentiate
	// privacy-enabled IPv6 addresses here. Therefore, we hope the preferred addresses are listed
	// first, and we never list link-local addresses.

	// looklook: collect into a temporary buffer before sorting so truncation at `count` never drops LAN IPs.
#define LOOKLOOK_MAX_COLLECT 64
	addr_record_t all[LOOKLOOK_MAX_COLLECT];
	int ranks[LOOKLOOK_MAX_COLLECT];
	size_t found = 0;
	uint32_t def_v4 = 0;
	bool has_def_v4 = looklook_default_ipv4(&def_v4);
	int ret = 0;

#define LOOKLOOK_PUSH_ADDR(sa_ptr, sa_len, unmap_v4mapped, has_gw)                                 \
	do {                                                                                           \
		if (!has_duplicate_addr((struct sockaddr *)(sa_ptr), all, found)) {                        \
			++ret;                                                                                 \
			if (found < LOOKLOOK_MAX_COLLECT) {                                                    \
				memcpy(&all[found].addr, (sa_ptr), (sa_len));                                      \
				all[found].len = (sa_len);                                                         \
				if (unmap_v4mapped)                                                                \
					addr_unmap_inet6_v4mapped((struct sockaddr *)&all[found].addr,                 \
					                          &all[found].len);                                    \
				addr_set_port((struct sockaddr *)&all[found].addr, port);                          \
				ranks[found] = looklook_rank(&all[found], def_v4, has_def_v4, (has_gw));           \
				++found;                                                                           \
			}                                                                                      \
		}                                                                                          \
	} while (0)

#if JUICE_ENABLE_LOCALHOST_ADDRESS
	// Add localhost for test purposes
	addr_record_t local;
	if (bound.addr.ss_family == AF_INET6 && udp_get_local_addr(sock, AF_INET6, &local) == 0) {
		++ret;
		if (found < LOOKLOOK_MAX_COLLECT) {
			all[found] = local;
			ranks[found] = looklook_rank(&all[found], def_v4, has_def_v4, true);
			++found;
		}
	}
	if (udp_get_local_addr(sock, AF_INET, &local) == 0) {
		++ret;
		if (found < LOOKLOOK_MAX_COLLECT) {
			all[found] = local;
			ranks[found] = looklook_rank(&all[found], def_v4, has_def_v4, true);
			++found;
		}
	}
#endif

#ifdef _WIN32
	// looklook: use GetAdaptersAddresses so we can skip down/tunnel/virtual adapters (WSL, Hyper-V,
	// Docker, VMware, VirtualBox, Clash) and prioritize adapters with a default gateway.
	bool got_adapters = false;
	ULONG aa_flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER |
	                 GAA_FLAG_INCLUDE_GATEWAYS;
	ULONG aa_len = 16384;
	PIP_ADAPTER_ADDRESSES aa_buf = (PIP_ADAPTER_ADDRESSES)malloc(aa_len);
	if (aa_buf) {
		ULONG aa_ret = GetAdaptersAddresses(AF_UNSPEC, aa_flags, NULL, aa_buf, &aa_len);
		if (aa_ret == ERROR_BUFFER_OVERFLOW) {
			free(aa_buf);
			aa_buf = (PIP_ADAPTER_ADDRESSES)malloc(aa_len);
			if (aa_buf)
				aa_ret = GetAdaptersAddresses(AF_UNSPEC, aa_flags, NULL, aa_buf, &aa_len);
		}
		if (aa_ret == NO_ERROR && aa_buf) {
			got_adapters = true;
			for (PIP_ADAPTER_ADDRESSES ad = aa_buf; ad; ad = ad->Next) {
				if (ad->OperStatus != IfOperStatusUp)
					continue;
				if (ad->IfType == IF_TYPE_SOFTWARE_LOOPBACK || ad->IfType == IF_TYPE_TUNNEL)
					continue;
				if (looklook_skip_win_adapter(ad->FriendlyName, ad->Description))
					continue;
				bool has_gw = ad->FirstGatewayAddress != NULL;
				for (PIP_ADAPTER_UNICAST_ADDRESS ua = ad->FirstUnicastAddress; ua; ua = ua->Next) {
					struct sockaddr *sa = ua->Address.lpSockaddr;
					socklen_t len = (socklen_t)ua->Address.iSockaddrLength;
					if (sa &&
					    (sa->sa_family == AF_INET ||
					     (sa->sa_family == AF_INET6 && bound.addr.ss_family == AF_INET6)) &&
					    !addr_is_local(sa) && !looklook_skip_addr(sa) && len > 0) {
						LOOKLOOK_PUSH_ADDR(sa, len, true, has_gw);
					}
				}
			}
		}
		free(aa_buf);
	}
	if (!got_adapters) {
		char buf[4096];
		DWORD len = 0;
		if (WSAIoctl(sock, SIO_ADDRESS_LIST_QUERY, NULL, 0, buf, sizeof(buf), &len, NULL, NULL)) {
			JLOG_ERROR("WSAIoctl with SIO_ADDRESS_LIST_QUERY failed, errno=%d", WSAGetLastError());
			return -1;
		}

		SOCKET_ADDRESS_LIST *list = (SOCKET_ADDRESS_LIST *)buf;
		for (int i = 0; i < list->iAddressCount; ++i) {
			struct sockaddr *sa = list->Address[i].lpSockaddr;
			socklen_t sa_len = list->Address[i].iSockaddrLength;
			if ((sa->sa_family == AF_INET ||
			     (sa->sa_family == AF_INET6 && bound.addr.ss_family == AF_INET6)) &&
			    !addr_is_local(sa) && !looklook_skip_addr(sa)) { // looklook: skip fake-ip
				LOOKLOOK_PUSH_ADDR(sa, sa_len, true, true);
			}
		}
	}
#else // POSIX
#ifndef NO_IFADDRS
	struct ifaddrs *ifas;
	if (getifaddrs(&ifas)) {
		JLOG_ERROR("getifaddrs failed, errno=%d", sockerrno);
		return -1;
	}

	for (struct ifaddrs *ifa = ifas; ifa; ifa = ifa->ifa_next) {
		unsigned int flags = ifa->ifa_flags;
		if (!(flags & IFF_UP) || !(flags & IFF_RUNNING) || (flags & IFF_LOOPBACK))
			continue;
		if (strcmp(ifa->ifa_name, "docker0") == 0)
			continue;
		if (looklook_skip_ifname(ifa->ifa_name)) // looklook
			continue;

		struct sockaddr *sa = ifa->ifa_addr;
		socklen_t len;
		if (sa &&
		    (sa->sa_family == AF_INET ||
		     (sa->sa_family == AF_INET6 && bound.addr.ss_family == AF_INET6)) &&
		    !addr_is_local(sa) && !looklook_skip_addr(sa) && (len = addr_get_len(sa)) > 0) { // looklook
			LOOKLOOK_PUSH_ADDR(sa, len, false, true);
		}
	}

	freeifaddrs(ifas);

#else // NO_IFADDRS defined
	char buf[4096];
	struct ifconf ifc;
	memset(&ifc, 0, sizeof(ifc));
	ifc.ifc_len = sizeof(buf);
	ifc.ifc_buf = buf;

	if (ioctlsocket(sock, SIOCGIFCONF, &ifc)) {
		JLOG_ERROR("ioctl for SIOCGIFCONF failed, errno=%d", sockerrno);
		return -1;
	}

	bool ifconf_has_inet6 = false;
	int n = ifc.ifc_len / sizeof(struct ifreq);
	for (int i = 0; i < n; ++i) {
		struct ifreq *ifr = ifc.ifc_req + i;
		struct sockaddr *sa = &ifr->ifr_addr;
		if (looklook_skip_ifname(ifr->ifr_name)) // looklook
			continue;
		if (sa->sa_family == AF_INET6)
			ifconf_has_inet6 = true;

		socklen_t len;
		if ((sa->sa_family == AF_INET ||
		     (sa->sa_family == AF_INET6 && bound.addr.ss_family == AF_INET6)) &&
		    !addr_is_local(sa) && !looklook_skip_addr(sa) && (len = addr_get_len(sa)) > 0) { // looklook
			LOOKLOOK_PUSH_ADDR(sa, len, false, true);
		}
	}

	if (!ifconf_has_inet6 && bound.addr.ss_family == AF_INET6) {
		struct sockaddr_in6 sin6;
		if (get_local_default_inet6(port, &sin6) == 0) {
			if (!addr_is_local((const struct sockaddr *)&sin6)) {
				LOOKLOOK_PUSH_ADDR(&sin6, sizeof(sin6), false, true);
			}
		}
	}
#endif
#endif

	looklook_sort(all, ranks, found); // looklook
	size_t copy_n = found < count ? found : count;
	if (copy_n > 0)
		memcpy(records, all, copy_n * sizeof(addr_record_t));
#undef LOOKLOOK_PUSH_ADDR
#undef LOOKLOOK_MAX_COLLECT
	return (int)copy_n;
}
