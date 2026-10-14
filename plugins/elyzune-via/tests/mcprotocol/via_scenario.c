/*
 * via_scenario: a two-bot mcprotocol.c scenario for elyzune-via.
 *
 *   via_scenario <protocol> <host> <port> <name-prefix> [rich] [names:<file>]
 *                [switch:<to>:<back>]
 *
 * Two offline bots speaking <protocol> connect to <host>:<port> (Infrarust
 * with elyzune-via in front of a 26.2 Hyperion backend) and walk the steps
 * below. Each step prints one line:
 *
 *   STEP <protocol> <name> PASS|FAIL <detail>
 *
 * The bots log in and configure the way a vanilla client does instead of
 * using mc_client_connect(): they answer select_known_packs with the core
 * pack of their own release only when it is offered (a vanilla client never
 * claims a pack it was not offered), so the known-packs translation is what
 * the test proves rather than something a scripted shortcut hides.
 *
 * Every clientbound packet the scenario relies on is decoded with
 * mcprotocol.c's release-aware readers for <protocol>, and the readers must
 * consume the whole body: a translated packet that only "looks right" fails.
 * Beyond those, every play packet of a family mcprotocol.c has a typed
 * decoder for (entity data, slots, chunks, movement, time, attributes, ...)
 * goes through mc_decode_packet() in vanilla-compatible mode (what a vanilla
 * client accepts); the "decode" step fails if any of them does not decode
 * exactly.
 *
 * With "rich" the first bot must be an operator of a vanilla backend: it
 * then runs commands that make the server send item components, entity data,
 * particles, sounds, time, teams and beds, so those translations are decoded
 * too (Hyperion's test events send few of them).
 *
 * With "names:<file>" (three lines: the release's slot displays, recipe
 * displays and argument parsers, comma separated, in id order) the packets
 * mcprotocol.c has no reader for -- recipe book entries, recipes,
 * advancements, the command graph, block entity data -- are decoded by this
 * program to their last byte, and "rich" adds `/recipe give @s *`, an
 * advancement grant, a hovered tellraw and signed/patterned/sherded block
 * entities: the step "rich_exact".
 *
 * With "switch:<to>:<back>" the first bot runs `/ir server <to>` and then
 * `/ir server <back>`: Infrarust moves it to another backend through a
 * second configuration phase (start_configuration, new registries, a new
 * login), which builds a fresh translator for the new backend connection
 * while the client keeps its socket. Both servers need `server` among the
 * proxy's player commands.
 *
 * Exit status: 0 when every step passed, 1 otherwise.
 */

#define _POSIX_C_SOURCE 200809L

#include "api.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define MAX_SEEN 256

typedef struct {
    McClient *client;
    int protocol;
    char name[17];
    const char *release; /* core known-pack id of this release */

    /* login / configuration */
    bool login_success;
    int pending_compression; /* -2: none pending */
    bool kicked;
    char kick[256];
    bool known_packs_offered;
    bool own_core_offered;
    int registry_packets;
    int tag_packets;
    bool config_finished;
    unsigned char reply[256]; /* pending configuration reply body */
    size_t reply_size;
    const char *reply_name;

    /* play */
    bool joined;
    bool join_decoded;
    int32_t entity_id;
    int chunks;
    int chunk_bytes_min;
    bool have_position;
    McPlayerPosition position;
    int32_t seen[MAX_SEEN];
    int32_t seen_types[MAX_SEEN];
    int seen_count;
    int spawns_bad;
    const char *chat_needle;
    bool saw_chat;
    int system_chats_bad;
    int container_contents;
    int32_t container_state;

    /* typed decoding of every play packet with a decoder */
    int decoded;
    int decode_failed;
    char decode_failures[256];
    int set_slots;
    int metadata_packets;
    int particles;
    int time_updates;
    int block_entities;
    int sounds;
    int teams;
    bool dead;
    double last_heartbeat;

    /* server switch: start_configuration seen, finished reconfigurations */
    bool reconfigure;
    int switches;

    /* exact decoding of the families mcprotocol.c has no reader for:
     * recipe book entries, recipes, advancements, the command graph,
     * block entity data (see the decoders below) */
    int recipe_entries;
    int recipe_packets, recipes_packets, advancement_packets, command_packets, tile_packets;
    int exact_failed;
    char exact_failures[256];
    bool hover_seen;
} Bot;

/* The client release's registries in id order, from the names file. */
#define MAX_NAMES 128
typedef struct {
    char *names[MAX_NAMES];
    int count;
} Names;

static Names slot_displays, recipe_displays, parsers;

/* Typed decoder output; every family's decoded struct fits. */
static unsigned char decode_storage[1 << 16] __attribute__((aligned(16)));

static int failures = 0;

static void step(int protocol, const char *name, bool ok, const char *detail)
{
    printf("STEP %d %s %s %s\n", protocol, name, ok ? "PASS" : "FAIL", detail);
    fflush(stdout);
    if (!ok) failures++;
}

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1.0e6;
}

static const char *release_of(int protocol)
{
    switch (protocol) {
    case 774: return "1.21.11";
    case 775: return "26.1.2";
    case 776: return "26.2";
    case 777: return "26.3";
    default: return "";
    }
}

/* The player entity type id of each release (registries.json). */
static int32_t player_type_of(int protocol)
{
    switch (protocol) {
    case 774: case 775: return 155;
    case 776: return 156;
    case 777: return 159;
    default: return -1;
    }
}

static bool bytes_eq(McBytes b, const char *s)
{
    size_t n = strlen(s);
    return b.size == n && memcmp(b.data, s, n) == 0;
}

static bool contains(const unsigned char *hay, size_t n, const char *needle)
{
    size_t m = strlen(needle);
    if (m == 0 || n < m) return false;
    for (size_t i = 0; i + m <= n; ++i) {
        if (memcmp(hay + i, needle, m) == 0) return true;
    }
    return false;
}

static void on_login(Bot *b, const char *name, const unsigned char *p, size_t n)
{
    McReader r;
    mc_reader_init(&r, p, n);
    if (strcmp(name, "compress") == 0) {
        int32_t threshold = 0;
        if (mc_reader_varint(&r, &threshold)) b->pending_compression = threshold;
    } else if (strcmp(name, "success") == 0) {
        b->login_success = true;
    } else if (strcmp(name, "disconnect") == 0) {
        b->kicked = true;
        snprintf(b->kick, sizeof b->kick, "login disconnect (%zu bytes)", n);
    }
}

static void on_config(Bot *b, const char *name, const unsigned char *p, size_t n)
{
    McReader r;
    mc_reader_init(&r, p, n);
    if (strcmp(name, "select_known_packs") == 0) {
        /* Answer like a vanilla client: select only the offered packs this
         * release ships, which is the core pack of its own version. */
        int32_t count = 0;
        b->known_packs_offered = true;
        McPacket reply;
        mc_packet_init(&reply, b->reply, sizeof b->reply);
        int32_t selected = 0;
        unsigned char body[192];
        McPacket packs;
        mc_packet_init(&packs, body, sizeof body);
        if (mc_reader_varint(&r, &count)) {
            for (int32_t i = 0; i < count; ++i) {
                McBytes ns, id, version;
                if (!mc_reader_string(&r, &ns) || !mc_reader_string(&r, &id)
                    || !mc_reader_string(&r, &version)) break;
                if (bytes_eq(ns, "minecraft") && bytes_eq(id, "core")
                    && bytes_eq(version, b->release)) {
                    b->own_core_offered = true;
                    mc_packet_string(&packs, "minecraft");
                    mc_packet_string(&packs, "core");
                    mc_packet_string(&packs, b->release);
                    selected++;
                }
            }
        }
        mc_packet_varint(&reply, selected);
        mc_packet_bytes(&reply, body, packs.length);
        b->reply_size = reply.length;
        b->reply_name = "select_known_packs";
    } else if (strcmp(name, "registry_data") == 0) {
        b->registry_packets++;
    } else if (strcmp(name, "tags") == 0) {
        b->tag_packets++;
    } else if (strcmp(name, "keep_alive") == 0 && n <= sizeof b->reply) {
        memcpy(b->reply, p, n);
        b->reply_size = n;
        b->reply_name = "keep_alive";
    } else if (strcmp(name, "ping") == 0 && n <= sizeof b->reply) {
        memcpy(b->reply, p, n);
        b->reply_size = n;
        b->reply_name = "pong";
    } else if (strcmp(name, "finish_configuration") == 0) {
        b->config_finished = true;
    } else if (strcmp(name, "disconnect") == 0) {
        b->kicked = true;
        snprintf(b->kick, sizeof b->kick, "configuration disconnect (%zu bytes)", n);
    }
}

/* ---- exact decoders for recipes, advancements, commands, block entities ----
 *
 * Written from the releases' sources for 1.21.11 to 26.3; a translated
 * packet must decode to its last byte. Registry ids are resolved to names
 * with the release's own lists (names file), which decide each value's
 * shape. */

static bool parse_names(char *list, Names *names)
{
    names->count = 0;
    for (char *save = NULL, *name = strtok_r(list, ",", &save); name != NULL;
         name = strtok_r(NULL, ",", &save)) {
        if (names->count >= MAX_NAMES) return false;
        names->names[names->count++] = name;
    }
    return names->count > 0;
}

static bool load_names(const char *path)
{
    FILE *file = fopen(path, "r");
    if (file == NULL) return false;
    static char lines[3][8192];
    bool ok = true;
    for (int i = 0; i < 3 && ok; ++i) ok = fgets(lines[i], sizeof lines[i], file) != NULL;
    fclose(file);
    if (!ok) return false;
    for (int i = 0; i < 3; ++i) lines[i][strcspn(lines[i], "\n")] = '\0';
    return parse_names(lines[0], &slot_displays) && parse_names(lines[1], &recipe_displays)
        && parse_names(lines[2], &parsers);
}

static const char *name_at(const Names *names, int32_t id)
{
    return id >= 0 && id < names->count ? names->names[id] : "?";
}

/* Skips one network NBT value. */
static bool nbt_skip_payload(McReader *r, uint8_t type, int depth)
{
    if (depth > 64) return false;
    switch (type) {
    case 1: return mc_reader_skip(r, 1);
    case 2: return mc_reader_skip(r, 2);
    case 3: case 5: return mc_reader_skip(r, 4);
    case 4: case 6: return mc_reader_skip(r, 8);
    case 7: case 11: case 12: {
        int32_t n = 0;
        if (!mc_reader_i32(r, &n) || n < 0) return false;
        size_t width = type == 7 ? 1 : type == 11 ? 4 : 8;
        return (size_t)n <= mc_reader_remaining(r) / width && mc_reader_skip(r, (size_t)n * width);
    }
    case 8: {
        uint16_t n = 0;
        return mc_reader_u16(r, &n) && mc_reader_skip(r, n);
    }
    case 9: {
        uint8_t element = 0;
        int32_t n = 0;
        if (!mc_reader_u8(r, &element) || !mc_reader_i32(r, &n) || n < 0) return false;
        for (int32_t i = 0; i < n; ++i) {
            if (!nbt_skip_payload(r, element, depth + 1)) return false;
        }
        return true;
    }
    case 10:
        for (;;) {
            uint8_t child = 0;
            uint16_t key = 0;
            if (!mc_reader_u8(r, &child)) return false;
            if (child == 0) return true;
            if (!mc_reader_u16(r, &key) || !mc_reader_skip(r, key)) return false;
            if (!nbt_skip_payload(r, child, depth + 1)) return false;
        }
    default:
        return false;
    }
}

static bool nbt_skip(McReader *r)
{
    uint8_t type = 0;
    if (!mc_reader_u8(r, &type)) return false;
    return type == 0 || nbt_skip_payload(r, type, 0);
}

/* ItemStackTemplate (26.1+): id, count, patch; 1.21.11 nests full stacks.
 * The patch is validated with mcprotocol.c's stack reader. */
static bool skip_template(McReader *r, int protocol)
{
    McItemStackView view;
    if (protocol == 774) return mc_reader_item_stack(r, protocol, MC_ITEM_WIRE_FULL, &view);
    int32_t id = 0, count = 0;
    if (!mc_reader_varint(r, &id) || !mc_reader_varint(r, &count)) return false;
    static unsigned char buffer[1 << 16];
    McPacket head;
    mc_packet_init(&head, buffer, sizeof buffer);
    mc_packet_varint(&head, count > 0 ? count : 1);
    mc_packet_varint(&head, id);
    size_t header = head.length;
    /* Only the stack has to fit, not the rest of the packet: a full recipe
     * book is far larger than one stack. A stack past the window fails. */
    size_t rest = mc_reader_remaining(r);
    if (rest > sizeof buffer - header) rest = sizeof buffer - header;
    memcpy(buffer + header, r->data + r->offset, rest);
    McReader synthetic;
    mc_reader_init(&synthetic, buffer, header + rest);
    if (!mc_reader_item_stack(&synthetic, protocol, MC_ITEM_WIRE_FULL, &view)) return false;
    return mc_reader_skip(r, synthetic.offset - header);
}

static bool skip_holder_set(McReader *r)
{
    int32_t n = 0;
    if (!mc_reader_varint(r, &n) || n < 0) return false;
    if (n == 0) {
        McBytes tag;
        return mc_reader_string(r, &tag);
    }
    for (int32_t i = 0; i < n - 1; ++i) {
        int32_t id = 0;
        if (!mc_reader_varint(r, &id)) return false;
    }
    return true;
}

static bool skip_slot_display(McReader *r, int protocol, int depth)
{
    int32_t type = 0;
    if (depth > 16 || !mc_reader_varint(r, &type)) return false;
    const char *name = name_at(&slot_displays, type);
    if (strcmp(name, "minecraft:empty") == 0 || strcmp(name, "minecraft:any_fuel") == 0) return true;
    if (strcmp(name, "minecraft:with_any_potion") == 0) return skip_slot_display(r, protocol, depth + 1);
    if (strcmp(name, "minecraft:only_with_component") == 0) {
        int32_t component = 0;
        return skip_slot_display(r, protocol, depth + 1) && mc_reader_varint(r, &component);
    }
    if (strcmp(name, "minecraft:item") == 0) {
        int32_t item = 0;
        return mc_reader_varint(r, &item);
    }
    if (strcmp(name, "minecraft:item_stack") == 0) return skip_template(r, protocol);
    if (strcmp(name, "minecraft:tag") == 0) {
        if (protocol == 777) return skip_holder_set(r);
        McBytes tag;
        return mc_reader_string(r, &tag);
    }
    if (strcmp(name, "minecraft:dyed") == 0 || strcmp(name, "minecraft:with_remainder") == 0) {
        return skip_slot_display(r, protocol, depth + 1) && skip_slot_display(r, protocol, depth + 1);
    }
    if (strcmp(name, "minecraft:smithing_trim") == 0) {
        if (!skip_slot_display(r, protocol, depth + 1) || !skip_slot_display(r, protocol, depth + 1)) return false;
        int32_t pattern = 0;
        if (!mc_reader_varint(r, &pattern)) return false;
        if (pattern != 0) return true;
        McBytes asset;
        bool decal = false;
        return mc_reader_string(r, &asset) && nbt_skip(r) && mc_reader_bool(r, &decal);
    }
    if (strcmp(name, "minecraft:composite") == 0) {
        int32_t n = 0;
        if (!mc_reader_varint(r, &n) || n < 0) return false;
        for (int32_t i = 0; i < n; ++i) {
            if (!skip_slot_display(r, protocol, depth + 1)) return false;
        }
        return true;
    }
    return false;
}

static bool skip_recipe_display(McReader *r, int protocol)
{
    int32_t type = 0;
    if (!mc_reader_varint(r, &type)) return false;
    const char *name = name_at(&recipe_displays, type);
    int count = 0;
    if (strcmp(name, "minecraft:crafting_shapeless") == 0) {
        int32_t n = 0;
        if (!mc_reader_varint(r, &n) || n < 0) return false;
        count = n + 2;
    } else if (strcmp(name, "minecraft:crafting_shaped") == 0) {
        int32_t w = 0, h = 0, n = 0;
        if (!mc_reader_varint(r, &w) || !mc_reader_varint(r, &h) || !mc_reader_varint(r, &n) || n < 0) return false;
        count = n + 2;
    } else if (strcmp(name, "minecraft:furnace") == 0) {
        count = 4;
    } else if (strcmp(name, "minecraft:stonecutter") == 0) {
        count = 3;
    } else if (strcmp(name, "minecraft:smithing") == 0) {
        count = 5;
    } else {
        return false;
    }
    for (int i = 0; i < count; ++i) {
        if (!skip_slot_display(r, protocol, 0)) return false;
    }
    if (strcmp(name, "minecraft:furnace") == 0) {
        int32_t time = 0;
        float xp = 0;
        return mc_reader_varint(r, &time) && mc_reader_float(r, &xp);
    }
    return true;
}

static bool exact_recipe_book_add(Bot *b, McReader *r)
{
    int32_t entries = 0;
    if (!mc_reader_varint(r, &entries) || entries < 0) return false;
    for (int32_t i = 0; i < entries; ++i) {
        int32_t display = 0, group = 0, category = 0;
        bool requirements = false;
        uint8_t flags = 0;
        if (!mc_reader_varint(r, &display) || !skip_recipe_display(r, b->protocol)
            || !mc_reader_varint(r, &group) || !mc_reader_varint(r, &category)
            || !mc_reader_bool(r, &requirements)) return false;
        if (requirements) {
            int32_t n = 0;
            if (!mc_reader_varint(r, &n) || n < 0) return false;
            for (int32_t j = 0; j < n; ++j) {
                if (!skip_holder_set(r)) return false;
            }
        }
        if (!mc_reader_u8(r, &flags)) return false;
        b->recipe_entries++;
    }
    bool replace = false;
    return mc_reader_bool(r, &replace);
}

/* update_recipes: Map<Identifier, List<VarInt item>> property sets, then
 * stonecutter recipes: List<{Ingredient, SlotDisplay}>. */
static bool exact_update_recipes(Bot *b, McReader *r)
{
    int32_t sets = 0;
    if (!mc_reader_varint(r, &sets) || sets < 0) return false;
    for (int32_t i = 0; i < sets; ++i) {
        McBytes key;
        int32_t n = 0;
        if (!mc_reader_string(r, &key) || !mc_reader_varint(r, &n) || n < 0) return false;
        for (int32_t j = 0; j < n; ++j) {
            int32_t item = 0;
            if (!mc_reader_varint(r, &item)) return false;
        }
    }
    int32_t stonecutter = 0;
    if (!mc_reader_varint(r, &stonecutter) || stonecutter < 0) return false;
    for (int32_t i = 0; i < stonecutter; ++i) {
        if (!skip_holder_set(r) || !skip_slot_display(r, b->protocol, 0)) return false;
    }
    return true;
}

static bool skip_string_list(McReader *r)
{
    int32_t n = 0;
    if (!mc_reader_varint(r, &n) || n < 0) return false;
    for (int32_t i = 0; i < n; ++i) {
        McBytes s;
        if (!mc_reader_string(r, &s)) return false;
    }
    return true;
}

static bool exact_advancements(Bot *b, McReader *r)
{
    bool reset = false;
    int32_t count = 0;
    if (!mc_reader_bool(r, &reset) || !mc_reader_varint(r, &count) || count < 0) return false;
    for (int32_t i = 0; i < count; ++i) {
        McBytes id;
        bool parent = false, display = false, telemetry = false;
        if (!mc_reader_string(r, &id) || !mc_reader_bool(r, &parent)) return false;
        if (parent) {
            McBytes parent_id;
            if (!mc_reader_string(r, &parent_id)) return false;
        }
        if (!mc_reader_bool(r, &display)) return false;
        if (display) {
            int32_t frame = 0, flags = 0;
            if (!nbt_skip(r) || !nbt_skip(r) || !skip_template(r, b->protocol)
                || !mc_reader_varint(r, &frame) || !mc_reader_i32(r, &flags)) return false;
            if (flags & 1) {
                McBytes background;
                if (!mc_reader_string(r, &background)) return false;
            }
            if (b->protocol != 777 && !mc_reader_skip(r, 8)) return false;
        }
        int32_t requirements = 0;
        if (!mc_reader_varint(r, &requirements) || requirements < 0) return false;
        for (int32_t j = 0; j < requirements; ++j) {
            if (!skip_string_list(r)) return false;
        }
        if (!mc_reader_bool(r, &telemetry)) return false;
        if (b->protocol == 777 && !mc_reader_skip(r, 8)) return false;
    }
    if (!skip_string_list(r)) return false;
    int32_t progress = 0;
    if (!mc_reader_varint(r, &progress) || progress < 0) return false;
    for (int32_t i = 0; i < progress; ++i) {
        McBytes id;
        int32_t criteria = 0;
        if (!mc_reader_string(r, &id) || !mc_reader_varint(r, &criteria) || criteria < 0) return false;
        for (int32_t j = 0; j < criteria; ++j) {
            McBytes criterion;
            bool done = false;
            if (!mc_reader_string(r, &criterion) || !mc_reader_bool(r, &done)) return false;
            if (done && !mc_reader_skip(r, 8)) return false;
        }
    }
    bool show = false;
    return mc_reader_bool(r, &show);
}

static bool skip_parser_properties(McReader *r, const char *parser)
{
    size_t width = 0;
    if (strcmp(parser, "brigadier:float") == 0 || strcmp(parser, "brigadier:integer") == 0) width = 4;
    if (strcmp(parser, "brigadier:double") == 0 || strcmp(parser, "brigadier:long") == 0) width = 8;
    if (width != 0) {
        uint8_t flags = 0;
        if (!mc_reader_u8(r, &flags)) return false;
        if ((flags & 1) && !mc_reader_skip(r, width)) return false;
        return !(flags & 2) || mc_reader_skip(r, width);
    }
    if (strcmp(parser, "brigadier:string") == 0) {
        int32_t mode = 0;
        return mc_reader_varint(r, &mode);
    }
    if (strcmp(parser, "minecraft:entity") == 0 || strcmp(parser, "minecraft:score_holder") == 0) {
        return mc_reader_skip(r, 1);
    }
    if (strcmp(parser, "minecraft:time") == 0) return mc_reader_skip(r, 4);
    /* The registry-bound parsers carry the registry name; their sibling
     * minecraft:resource_location carries nothing, so no prefix match. */
    if (strcmp(parser, "minecraft:resource") == 0 || strcmp(parser, "minecraft:resource_key") == 0
        || strcmp(parser, "minecraft:resource_or_tag") == 0
        || strcmp(parser, "minecraft:resource_or_tag_key") == 0
        || strcmp(parser, "minecraft:resource_selector") == 0) {
        McBytes registry;
        return mc_reader_string(r, &registry);
    }
    return strcmp(parser, "?") != 0;
}

static bool exact_commands(McReader *r)
{
    int32_t count = 0;
    if (!mc_reader_varint(r, &count) || count < 0) return false;
    for (int32_t i = 0; i < count; ++i) {
        uint8_t flags = 0;
        int32_t children = 0;
        if (!mc_reader_u8(r, &flags) || !mc_reader_varint(r, &children) || children < 0) return false;
        for (int32_t c = 0; c < children; ++c) {
            int32_t child = 0;
            if (!mc_reader_varint(r, &child) || child < 0 || child >= count) return false;
        }
        if (flags & 0x08) {
            int32_t redirect = 0;
            if (!mc_reader_varint(r, &redirect) || redirect < 0 || redirect >= count) return false;
        }
        int kind = flags & 0x03;
        if (kind == 1 || kind == 2) {
            McBytes name;
            if (!mc_reader_string(r, &name)) return false;
        }
        if (kind == 2) {
            int32_t parser = 0;
            if (!mc_reader_varint(r, &parser) || !skip_parser_properties(r, name_at(&parsers, parser))) return false;
        }
        if (flags & 0x10) {
            McBytes suggestions;
            if (!mc_reader_string(r, &suggestions)) return false;
        }
    }
    int32_t root = 0;
    return mc_reader_varint(r, &root) && root >= 0 && root < count;
}

static void exact(Bot *b, const char *family, bool ok, McReader *r)
{
    if (ok && mc_reader_remaining(r) == 0) return;
    b->exact_failed++;
    size_t used = strlen(b->exact_failures);
    if (used + 48 < sizeof b->exact_failures) {
        snprintf(b->exact_failures + used, sizeof b->exact_failures - used, "%s@%zu ", family, r->offset);
    }
}

static void exact_decode(Bot *b, const char *name, const unsigned char *p, size_t n)
{
    if (slot_displays.count == 0) return;
    McReader r;
    mc_reader_init(&r, p, n);
    if (strcmp(name, "recipe_book_add") == 0) {
        b->recipe_packets++;
        exact(b, name, exact_recipe_book_add(b, &r), &r);
    } else if (strcmp(name, "declare_recipes") == 0) {
        b->recipes_packets++;
        exact(b, name, exact_update_recipes(b, &r), &r);
    } else if (strcmp(name, "advancements") == 0) {
        b->advancement_packets++;
        exact(b, name, exact_advancements(b, &r), &r);
    } else if (strcmp(name, "declare_commands") == 0) {
        b->command_packets++;
        exact(b, name, exact_commands(&r), &r);
    } else if (strcmp(name, "tile_entity_data") == 0) {
        McPosition at;
        int32_t type = 0;
        bool ok = mc_reader_position(&r, b->protocol, &at) && mc_reader_varint(&r, &type) && nbt_skip(&r);
        b->tile_packets++;
        exact(b, name, ok, &r);
    } else if (strcmp(name, "system_chat") == 0 && contains(p, n, "via-hover")
               && contains(p, n, "show_item")) {
        b->hover_seen = true;
    }
}

static void typed_decode(Bot *b, int32_t packet_id, const unsigned char *p, size_t n)
{
    McPacketFamily family = mc_packet_family(b->protocol, MC_STATE_PLAY, MC_PACKET_CLIENTBOUND, packet_id);
    if (family == MC_FAMILY_UNKNOWN || mc_packet_decoded_size(family) == 0) return;
    McError error;
    memset(&error, 0, sizeof error);
    McPacketFamily decoded_family = MC_FAMILY_UNKNOWN;
    int rc = mc_decode_packet(b->protocol, MC_STATE_PLAY, MC_PACKET_CLIENTBOUND, packet_id, p, n,
        MC_DECODE_VANILLA_COMPAT, decode_storage, sizeof decode_storage, &decoded_family, &error);
    if (rc == 0) {
        b->decoded++;
        return;
    }
    b->decode_failed++;
    size_t used = strlen(b->decode_failures);
    if (used + 48 < sizeof b->decode_failures) {
        snprintf(b->decode_failures + used, sizeof b->decode_failures - used, "%s:%s@%zu ",
            mc_packet_family_name(family), mc_error_name(error.code), error.offset);
    }
}

static void on_play(Bot *b, const char *name, int32_t packet_id, const unsigned char *p, size_t n)
{
    McReader r;
    mc_reader_init(&r, p, n);
    typed_decode(b, packet_id, p, n);
    exact_decode(b, name, p, n);
    if (strcmp(name, "set_slot") == 0 || strcmp(name, "set_player_inventory") == 0) b->set_slots++;
    else if (strcmp(name, "entity_metadata") == 0) b->metadata_packets++;
    /* Counted, not decoded: mcprotocol.c's world_particles reader rejects
     * valid untranslated 26.2 bodies (count 0), so it cannot judge the
     * translation; the golden tests cover the particle layouts. */
    else if (strcmp(name, "world_particles") == 0) b->particles++;
    else if (strcmp(name, "update_time") == 0) {
        McTimeUpdatePacket time;
        memset(&time, 0, sizeof time);
        if (mc_reader_clientbound_time_update(&r, b->protocol, &time) && mc_reader_remaining(&r) == 0) b->time_updates++;
        mc_reader_init(&r, p, n);
    } else if (strcmp(name, "tile_entity_data") == 0) b->block_entities++;
    else if (strcmp(name, "sound_effect") == 0 || strcmp(name, "entity_sound_effect") == 0) b->sounds++;
    else if (strcmp(name, "teams") == 0) b->teams++;
    else if (strcmp(name, "start_configuration") == 0) b->reconfigure = true;
    if (strcmp(name, "login") == 0) {
        McJoinGamePacket join;
        memset(&join, 0, sizeof join);
        b->joined = true;
        if (mc_reader_clientbound_join_game(&r, b->protocol, &join)
            && mc_reader_remaining(&r) == 0) {
            b->join_decoded = true;
            b->entity_id = join.entity_id;
        }
    } else if (strcmp(name, "map_chunk") == 0) {
        b->chunks++;
        if (b->chunk_bytes_min == 0 || (int)n < b->chunk_bytes_min) b->chunk_bytes_min = (int)n;
    } else if (strcmp(name, "position") == 0) {
        McClientboundPlayerPosition pos;
        memset(&pos, 0, sizeof pos);
        if (mc_reader_clientbound_player_position(&r, b->protocol, &pos)) {
            b->position = pos.position;
            b->have_position = true;
        }
    } else if (strcmp(name, "spawn_entity") == 0) {
        McClientboundEntitySpawn spawn;
        memset(&spawn, 0, sizeof spawn);
        if (mc_reader_clientbound_entity_spawn(&r, b->protocol, &spawn)
            && mc_reader_remaining(&r) == 0) {
            if (spawn.entity_type == player_type_of(b->protocol) && b->seen_count < MAX_SEEN) {
                b->seen[b->seen_count] = spawn.entity_id;
                b->seen_types[b->seen_count] = spawn.entity_type;
                b->seen_count++;
            }
        } else {
            b->spawns_bad++;
        }
    } else if (strcmp(name, "system_chat") == 0 || strcmp(name, "player_chat") == 0
        || strcmp(name, "profileless_chat") == 0) {
        if (strcmp(name, "system_chat") == 0) {
            McClientboundSystemChat chat;
            memset(&chat, 0, sizeof chat);
            if (!mc_reader_clientbound_system_chat(&r, b->protocol, &chat)
                || mc_reader_remaining(&r) != 0) b->system_chats_bad++;
        }
        if (b->chat_needle != NULL && contains(p, n, b->chat_needle)) b->saw_chat = true;
    } else if (strcmp(name, "window_items") == 0) {
        McContainerContent content;
        memset(&content, 0, sizeof content);
        if (mc_reader_container_content(&r, b->protocol, &content)) {
            b->container_contents++;
            b->container_state = content.state_id;
        }
    } else if (strcmp(name, "update_health") == 0) {
        /* A bot killed in an earlier run (or now) must respawn, or the
         * server stops sending it chunks. */
        float health = 1.0F;
        if (mc_reader_float(&r, &health) && health <= 0.0F) b->dead = true;
    } else if (strcmp(name, "kick_disconnect") == 0) {
        b->kicked = true;
        snprintf(b->kick, sizeof b->kick, "play disconnect (%zu bytes)", n);
    }
}

static void on_packet(void *userdata, McState state, int32_t packet_id,
    const unsigned char *payload, size_t payload_size)
{
    Bot *b = userdata;
    const char *name = mc_packet_name(b->protocol, state, MC_PACKET_CLIENTBOUND, packet_id);
    if (getenv("VIA_SCENARIO_TRACE") != NULL) {
        fprintf(stderr, "%s %d %s (%zu)\n", b->name, (int)state, name != NULL ? name : "?", payload_size);
    }
    if (name == NULL) return;
    switch (state) {
    case MC_STATE_LOGIN: on_login(b, name, payload, payload_size); break;
    case MC_STATE_CONFIGURATION: on_config(b, name, payload, payload_size); break;
    case MC_STATE_PLAY: on_play(b, name, packet_id, payload, payload_size); break;
    default: break;
    }
}

static void bot_init(Bot *b, int protocol, const char *name)
{
    memset(b, 0, sizeof *b);
    b->protocol = protocol;
    snprintf(b->name, sizeof b->name, "%s", name);
    b->release = release_of(protocol);
    b->pending_compression = -2;
}

static bool poll_once(Bot *b, unsigned int timeout_ms, char *error, size_t error_size)
{
    return mc_client_poll(b->client, timeout_ms, error, error_size) >= 0;
}

static bool configure(Bot *b, char *error, size_t error_size);

/* Handshake, login and configuration, the way a vanilla client does them. */
static bool bot_connect(Bot *b, const char *host, uint16_t port, char *error, size_t error_size)
{
    McCallbacks callbacks = {.on_packet = on_packet};
    b->client = mc_client_create(b->protocol, &callbacks, b, error, error_size);
    if (b->client == NULL) return false;
    if (mc_client_open(b->client, host, port, MC_STATE_LOGIN, error, error_size) != 0) return false;

    McUuid uuid;
    mc_offline_uuid(b->name, &uuid);
    unsigned char storage[64];
    McPacket start;
    mc_packet_init(&start, storage, sizeof storage);
    mc_packet_string(&start, b->name);
    mc_packet_uuid(&start, &uuid);
    if (mc_client_send_named(b->client, "login_start", start.data, start.length, error, error_size) != 0) return false;

    double deadline = now_ms() + 10000.0;
    while (!b->login_success && !b->kicked && now_ms() < deadline) {
        if (!poll_once(b, 500, error, error_size)) return false;
        if (b->pending_compression != -2) {
            mc_client_set_compression(b->client, b->pending_compression, error, error_size);
            b->pending_compression = -2;
        }
    }
    if (!b->login_success) {
        snprintf(error, error_size, "%s", b->kicked ? b->kick : "no login_finished");
        return false;
    }
    if (mc_client_send_named(b->client, "login_acknowledged", NULL, 0, error, error_size) != 0) return false;
    if (mc_client_set_state(b->client, MC_STATE_CONFIGURATION, error, error_size) != 0) return false;

    McClientInformation info = {
        .locale = "en_us", .view_distance = 4, .chat_mode = 0, .chat_colors = true,
        .skin_parts = 0x7F, .main_hand = 1, .text_filtering = false,
        .server_listing = true, .particle_status = 0,
    };
    if (mc_client_send_client_information(b->client, &info, error, error_size) != 0) return false;
    return configure(b, error, error_size);
}

/* Answers configuration packets until finish_configuration, acknowledges
 * it and enters play. The client must already be in the configuration
 * state. */
static bool configure(Bot *b, char *error, size_t error_size)
{
    double deadline = now_ms() + 15000.0;
    while (!b->config_finished && !b->kicked && now_ms() < deadline) {
        if (!poll_once(b, 500, error, error_size)) return false;
        if (b->reply_name != NULL) {
            if (mc_client_send_named(b->client, b->reply_name, b->reply, b->reply_size, error, error_size) != 0) return false;
            b->reply_name = NULL;
        }
    }
    if (!b->config_finished) {
        snprintf(error, error_size, "%s", b->kicked ? b->kick : "configuration did not finish");
        return false;
    }
    if (mc_client_send_named(b->client, "finish_configuration", NULL, 0, error, error_size) != 0) return false;
    return mc_client_set_state(b->client, MC_STATE_PLAY, error, error_size) == 0;
}

/* A server switch: the proxy sent start_configuration. Acknowledge it like a
 * vanilla client (no new client information; the proxy replays it), run the
 * new backend's configuration, and forget everything the old backend's world
 * told the bot. */
static bool bot_reconfigure(Bot *b, char *error, size_t error_size)
{
    b->reconfigure = false;
    b->config_finished = false;
    b->known_packs_offered = false;
    b->own_core_offered = false;
    b->registry_packets = 0;
    b->tag_packets = 0;
    b->joined = false;
    b->join_decoded = false;
    b->chunks = 0;
    b->have_position = false;
    b->seen_count = 0;
    if (mc_client_send_named(b->client, "configuration_acknowledged", NULL, 0, error, error_size) != 0) return false;
    if (mc_client_set_state(b->client, MC_STATE_CONFIGURATION, error, error_size) != 0) return false;
    if (!configure(b, error, error_size)) return false;
    b->switches++;
    return true;
}

/* Processes every packet already received by one bot (bounded), so a fast
 * backend cannot build a backlog that delays keepalive answers. Returns -1
 * on a socket error, otherwise the number of packets processed. */
static int drain(Bot *b, char *error, size_t error_size)
{
    int processed = 0;
    while (processed < 4096) {
        int rc = mc_client_poll(b->client, 0, error, error_size);
        if (rc < 0) return -1;
        if (rc == 0) break;
        processed++;
    }
    return processed;
}

/* A vanilla client sends its position at least once a second even when it
 * stands still, and the server re-evaluates which entities the player sees
 * on those updates (ChunkMap.move); a bot that never moves is never paired
 * with entities whose chunk it received after joining. */
static bool heartbeat(Bot *b, char *error, size_t error_size)
{
    if (b->client == NULL || !b->have_position || !b->joined) return true;
    double now = now_ms();
    if (now - b->last_heartbeat < 1000.0) return true;
    b->last_heartbeat = now;
    McPlayerPosition p = b->position;
    p.on_ground = true;
    return mc_client_send_player_position(b->client, &p, error, error_size) == 0;
}

/* Polls both bots until `done` holds or the timeout passes. */
static bool run_until(Bot *a, Bot *b, double timeout_ms, bool (*done)(Bot *, Bot *), char *error, size_t error_size)
{
    double deadline = now_ms() + timeout_ms;
    while (now_ms() < deadline) {
        if (done(a, b)) return true;
        int processed = 0;
        if (a != NULL && a->client != NULL) {
            int n = drain(a, error, error_size);
            if (n < 0) return false;
            processed += n;
        }
        if (b != NULL && b->client != NULL) {
            int n = drain(b, error, error_size);
            if (n < 0) return false;
            processed += n;
        }
        if ((a != NULL && a->kicked) || (b != NULL && b->kicked)) return false;
        Bot *bots[2] = {a, b};
        for (int i = 0; i < 2; ++i) {
            if (bots[i] != NULL && bots[i]->client != NULL && bots[i]->reconfigure
                && !bot_reconfigure(bots[i], error, error_size)) return false;
        }
        if ((a != NULL && !heartbeat(a, error, error_size)) || (b != NULL && !heartbeat(b, error, error_size))) return false;
        for (int i = 0; i < 2; ++i) {
            if (bots[i] != NULL && bots[i]->client != NULL && bots[i]->dead) {
                bots[i]->dead = false;
                if (mc_client_request_respawn(bots[i]->client, error, error_size) != 0) return false;
            }
        }
        if (processed == 0) {
            Bot *waiter = (a != NULL && a->client != NULL) ? a : b;
            if (waiter != NULL && mc_client_wait(waiter->client, 5, error, error_size) < 0) return false;
        }
    }
    return done(a, b);
}

static bool joined_with_chunks(Bot *a, Bot *b) { (void)b; return a->joined && a->chunks >= 4 && a->have_position; }
static bool a_saw_chat(Bot *a, Bot *b) { (void)b; return a->saw_chat; }
static bool never(Bot *a, Bot *b) { (void)a; (void)b; return false; }

static bool sees(Bot *watcher, Bot *peer)
{
    for (int i = 0; i < watcher->seen_count; ++i) {
        if (watcher->seen[i] == peer->entity_id && watcher->seen_types[i] == player_type_of(watcher->protocol)) return true;
    }
    return false;
}

static bool both_see(Bot *a, Bot *b) { return sees(a, b) && sees(b, a); }
static bool b_joined(Bot *a, Bot *b) { (void)a; return b->joined && b->have_position; }
static bool switched_once(Bot *a, Bot *b) { (void)b; return a->switches >= 1 && joined_with_chunks(a, b); }
static bool switched_back(Bot *a, Bot *b) { return a->switches >= 2 && joined_with_chunks(a, b) && sees(a, b); }

static bool send_chat(Bot *b, const char *text, char *error, size_t error_size)
{
    unsigned char storage[512];
    McPacket p;
    mc_packet_init(&p, storage, sizeof storage);
    mc_packet_string(&p, text);
    mc_packet_i64(&p, (int64_t)(now_ms()));
    mc_packet_i64(&p, 0);
    mc_packet_bool(&p, false);          /* no signature */
    mc_packet_varint(&p, 0);            /* last seen: offset */
    unsigned char acknowledged[3] = {0, 0, 0};
    mc_packet_bytes(&p, acknowledged, 3); /* 20-bit acknowledged set */
    mc_packet_u8(&p, 0);                /* checksum (1.21.5+) */
    return mc_client_send_named(b->client, "chat_message", p.data, p.length, error, error_size) == 0;
}

static bool send_command(Bot *b, const char *command)
{
    char error[256];
    return mc_client_send_command(b->client, command, error, sizeof error) == 0;
}

static bool rich_done(Bot *a, Bot *b)
{
    (void)b;
    bool beds = a->protocol == 777 || a->protocol == 776 || a->block_entities > 0;
    return a->set_slots >= 4 && a->particles >= 2 && a->time_updates >= 1 && a->sounds >= 1
        && a->teams >= 1 && beds && a->metadata_packets >= 2;
}

/* Commands whose answers carry item components, entity data, particles,
 * sounds, time, teams and a bed. Run by an operator on a vanilla backend. */
static void run_rich(Bot *a, Bot *b, int protocol)
{
    char error[256] = "";
    char detail[512];
    static const char *commands[] = {
        "give @s minecraft:diamond_sword[minecraft:custom_name='Blade',minecraft:lore=['line one'],"
            "minecraft:enchantments={'minecraft:sharpness':5},minecraft:dyed_color=16711680] 1",
        "give @s minecraft:bundle[minecraft:bundle_contents=[{id:'minecraft:stone',count:2}]] 1",
        "give @s minecraft:potion[minecraft:potion_contents={potion:'minecraft:healing'}] 1",
        "give @s minecraft:goat_horn[minecraft:instrument='minecraft:ponder_goat_horn'] 1",
        "give @s minecraft:leather_chestplate[minecraft:trim={material:'minecraft:gold',pattern:'minecraft:coast'}] 1",
        "summon minecraft:slime ~3 ~ ~ {Size:0,NoAI:1b}",
        "summon minecraft:villager ~-3 ~ ~ {NoAI:1b}",
        "summon minecraft:item_display ~ ~2 ~ {item:{id:'minecraft:diamond'}}",
        "particle minecraft:dust{color:[1.0,0.0,0.0],scale:1.0} ~ ~1 ~",
        "particle minecraft:item{item:{id:'minecraft:diamond'}} ~ ~1 ~",
        "particle minecraft:block{block_state:'minecraft:stone'} ~ ~1 ~",
        "playsound minecraft:entity.player.levelup master @s",
        "time set 6000",
        "team add elyzune",
        "team modify elyzune color gold",
        "team join elyzune @s",
        "setblock ~2 ~ ~2 minecraft:red_bed[part=head,facing=north]",
        "effect give @s minecraft:speed 30 1",
        /* Every recipe (recipe_book_add of all vanilla displays), an
         * advancement grant, a hovered chat line and block entities with
         * text, patterns, sherds and items. */
        "recipe give @s *",
        "advancement grant @s only minecraft:story/root",
        "tellraw @s {\"text\":\"via-hover\",\"hover_event\":{\"action\":\"show_item\","
            "\"id\":\"minecraft:diamond_sword\",\"components\":{\"minecraft:custom_name\":\"X\"}}}",
        "setblock ~4 ~ ~ minecraft:oak_sign{front_text:{messages:[{\"text\":\"a\",\"hover_event\":"
            "{\"action\":\"show_entity\",\"id\":\"minecraft:pig\",\"uuid\":[I;1,2,3,4]}},'',\"\",\"\"]}}",
        "setblock ~4 ~ ~2 minecraft:white_banner{patterns:[{pattern:'minecraft:stripe_top',color:'blue'}]}",
        "setblock ~4 ~ ~4 minecraft:decorated_pot{sherds:['minecraft:angler_pottery_sherd'],"
            "item:{id:'minecraft:diamond',count:1}}",
        "setblock ~4 ~ ~6 minecraft:campfire",
    };
    bool sent = true;
    for (size_t i = 0; i < sizeof commands / sizeof commands[0] && sent; ++i) {
        sent = send_command(a, commands[i]);
        run_until(a, b, 150.0, never, error, sizeof error);
    }
    bool done = sent && run_until(a, b, 10000.0, rich_done, error, sizeof error);
    snprintf(detail, sizeof detail,
        "slots=%d metadata=%d particles=%d time=%d sounds=%d teams=%d bed_entities=%d kicked=%d %s %s",
        a->set_slots, a->metadata_packets, a->particles, a->time_updates, a->sounds, a->teams,
        a->block_entities, a->kicked, a->kicked ? a->kick : "", error);
    step(protocol, "rich", done && !a->kicked, detail);
    snprintf(detail, sizeof detail, "decoded=%d failed=%d %s", a->decoded, a->decode_failed, a->decode_failures);
    step(protocol, "rich_decode", a->decode_failed == 0, detail);
    run_until(a, b, 2000.0, never, error, sizeof error);
    snprintf(detail, sizeof detail,
        "recipe_book_add=%d entries=%d update_recipes=%d advancements=%d commands=%d tile_entity_data=%d "
        "hover=%d failed=%d %s",
        a->recipe_packets, a->recipe_entries, a->recipes_packets, a->advancement_packets, a->command_packets,
        a->tile_packets, a->hover_seen, a->exact_failed, a->exact_failures);
    step(protocol, "rich_exact", a->exact_failed == 0 && a->recipe_entries > 100 && a->recipes_packets > 0
        && a->advancement_packets > 0 && a->command_packets > 0 && a->tile_packets >= 3 && a->hover_seen, detail);
}

int main(int argc, char **argv)
{
    if (argc < 5) {
        fprintf(stderr, "usage: %s <protocol> <host> <port> <name-prefix> [rich] [switch:<to>:<back>]\n", argv[0]);
        return 2;
    }
    int protocol = atoi(argv[1]);
    bool rich = false;
    char switch_to[64] = "", switch_back_to[64] = "";
    for (int i = 5; i < argc; ++i) {
        if (strcmp(argv[i], "rich") == 0) {
            rich = true;
        } else if (strncmp(argv[i], "names:", 6) == 0) {
            if (!load_names(argv[i] + 6)) {
                fprintf(stderr, "names file %s unreadable\n", argv[i] + 6);
                return 2;
            }
        } else if (strncmp(argv[i], "switch:", 7) == 0) {
            const char *to = argv[i] + 7;
            const char *colon = strchr(to, ':');
            if (colon == NULL || colon == to || colon[1] == '\0' || (size_t)(colon - to) >= sizeof switch_to
                || strlen(colon + 1) >= sizeof switch_back_to) {
                fprintf(stderr, "switch needs switch:<to>:<back>\n");
                return 2;
            }
            memcpy(switch_to, to, (size_t)(colon - to));
            switch_to[colon - to] = '\0';
            snprintf(switch_back_to, sizeof switch_back_to, "%s", colon + 1);
        }
    }
    const char *host = argv[2];
    uint16_t port = (uint16_t)atoi(argv[3]);
    char error[256] = "";
    char detail[512];
    char name_a[17], name_b[17];
    snprintf(name_a, sizeof name_a, "%sA%d", argv[4], protocol);
    snprintf(name_b, sizeof name_b, "%sB%d", argv[4], protocol);

    Bot a, b;
    bot_init(&a, protocol, name_a);
    bot_init(&b, protocol, name_b);

    /* 0. server list ping: a translated client must be told its own
     * protocol, or it shows the server as incompatible. */
    char json[8192] = "";
    McStatus status;
    memset(&status, 0, sizeof status);
    char expected_protocol[32];
    snprintf(expected_protocol, sizeof expected_protocol, "\"protocol\":%d", protocol);
    bool pinged = mc_status_ping(protocol, host, port, 5000, json, sizeof json, &status, error, sizeof error) == 0;
    snprintf(detail, sizeof detail, "pinged=%d reports_own_protocol=%d err=%s", pinged,
        strstr(json, expected_protocol) != NULL, error);
    step(protocol, "status", pinged && strstr(json, expected_protocol) != NULL, detail);
    error[0] = '\0';

    /* 1-2. login and configuration */
    bool connected = bot_connect(&a, host, port, error, sizeof error);
    snprintf(detail, sizeof detail, "login_finished=%d err=%s", a.login_success, error);
    step(protocol, "login", a.login_success, detail);
    snprintf(detail, sizeof detail, "registries=%d tags=%d known_packs_offered=%d own_core_offered=%d err=%s",
        a.registry_packets, a.tag_packets, a.known_packs_offered, a.own_core_offered, connected ? "" : error);
    step(protocol, "configuration", connected && a.registry_packets > 0 && a.own_core_offered, detail);
    if (!connected) return 1;

    /* 3-4. spawn and chunks */
    error[0] = '\0';
    bool spawned = run_until(&a, NULL, 60000.0, joined_with_chunks, error, sizeof error);
    snprintf(detail, sizeof detail, "join_decoded=%d entity=%d pos=(%.1f,%.1f,%.1f) err=%s",
        a.join_decoded, a.entity_id, a.position.x, a.position.y, a.position.z, a.kicked ? a.kick : error);
    step(protocol, "spawn", a.joined && a.join_decoded && a.have_position, detail);
    snprintf(detail, sizeof detail, "chunks=%d smallest=%d bytes", a.chunks, a.chunk_bytes_min);
    step(protocol, "chunks", spawned && a.chunks >= 4, detail);

    /* 5. move: a few position updates, then the connection must stay up */
    bool moved = true;
    for (int i = 0; i < 5 && moved; ++i) {
        McPlayerPosition p = a.position;
        p.x += 0.2 * (i + 1);
        p.on_ground = true;
        moved = mc_client_send_player_position(a.client, &p, error, sizeof error) == 0;
        run_until(&a, NULL, 100.0, never, error, sizeof error);
    }
    run_until(&a, NULL, 1000.0, never, error, sizeof error);
    snprintf(detail, sizeof detail, "sent=5 kicked=%d %s", a.kicked, a.kicked ? a.kick : "");
    step(protocol, "move", moved && !a.kicked, detail);

    /* 6. chat: the backend broadcasts chat as system chat */
    char needle[64];
    snprintf(needle, sizeof needle, "via-check-%d-%ld", protocol, (long)(now_ms()) % 100000L);
    a.chat_needle = needle;
    bool chatted = send_chat(&a, needle, error, sizeof error);
    bool echoed = chatted && run_until(&a, NULL, 5000.0, a_saw_chat, error, sizeof error);
    snprintf(detail, sizeof detail, "echoed=%d malformed_system_chat=%d", echoed, a.system_chats_bad);
    step(protocol, "chat", echoed && a.system_chats_bad == 0, detail);

    /* 7. hotbar and inventory click */
    bool hotbar = mc_client_select_hotbar_slot(a.client, 3, error, sizeof error) == 0;
    McWindowClick click = {.window_id = 0, .state_id = a.container_state, .slot = 36,
        .mouse_button = 0, .action_number = 0, .mode = 0};
    bool clicked = mc_client_click_window(a.client, &click, error, sizeof error) == 0;
    run_until(&a, NULL, 1500.0, never, error, sizeof error);
    snprintf(detail, sizeof detail, "hotbar=%d click=%d container_contents=%d kicked=%d %s",
        hotbar, clicked, a.container_contents, a.kicked, a.kicked ? a.kick : "");
    step(protocol, "inventory", hotbar && clicked && !a.kicked, detail);

    /* 8. a second bot: each must see the other's player entity */
    error[0] = '\0';
    bool b_connected = bot_connect(&b, host, port, error, sizeof error);
    bool b_in = b_connected && run_until(&a, &b, 60000.0, b_joined, error, sizeof error);
    bool seen = b_in && run_until(&a, &b, 60000.0, both_see, error, sizeof error);
    snprintf(detail, sizeof detail, "b_joined=%d a_sees_b=%d b_sees_a=%d a_spawns=%d b_spawns=%d bad_spawns=%d err=%s",
        b_in, sees(&a, &b), sees(&b, &a), a.seen_count, b.seen_count, a.spawns_bad + b.spawns_bad,
        b.kicked ? b.kick : error);
    step(protocol, "see_peer", seen && a.spawns_bad + b.spawns_bad == 0, detail);

    /* typed decoding of everything both bots received so far */
    snprintf(detail, sizeof detail, "decoded=%d failed=%d %s", a.decoded + b.decoded,
        a.decode_failed + b.decode_failed, a.decode_failures);
    step(protocol, "decode", a.decode_failed + b.decode_failed == 0 && a.decoded > 0, detail);

    if (rich) run_rich(&a, &b, protocol);

    /* server switch there and back: a new backend connection (and
     * translator) under the same client connection */
    if (switch_to[0] != '\0') {
        char command[96];
        error[0] = '\0';
        snprintf(command, sizeof command, "ir server %s", switch_to);
        bool switched = send_command(&a, command)
            && run_until(&a, &b, 60000.0, switched_once, error, sizeof error);
        snprintf(detail, sizeof detail, "to=%s switches=%d registries=%d own_core_offered=%d join_decoded=%d chunks=%d kicked=%d %s %s",
            switch_to, a.switches, a.registry_packets, a.own_core_offered, a.join_decoded, a.chunks, a.kicked,
            a.kicked ? a.kick : "", error);
        step(protocol, "switch", switched && a.join_decoded && a.own_core_offered, detail);

        error[0] = '\0';
        snprintf(command, sizeof command, "ir server %s", switch_back_to);
        bool back = switched && send_command(&a, command)
            && run_until(&a, &b, 60000.0, switched_back, error, sizeof error);
        snprintf(detail, sizeof detail, "to=%s switches=%d join_decoded=%d chunks=%d a_sees_b=%d kicked=%d %s %s",
            switch_back_to, a.switches, a.join_decoded, a.chunks, sees(&a, &b), a.kicked, a.kicked ? a.kick : "", error);
        step(protocol, "switch_back", back && a.join_decoded, detail);

        snprintf(detail, sizeof detail, "decoded=%d failed=%d %s", a.decoded, a.decode_failed, a.decode_failures);
        step(protocol, "switch_decode", a.decode_failed == 0, detail);
    }

    /* 9. disconnect, 10. reconnect with the same name */
    mc_client_disconnect(a.client);
    mc_client_destroy(a.client);
    a.client = NULL;
    run_until(NULL, &b, 1500.0, never, error, sizeof error);
    step(protocol, "disconnect", !b.kicked, b.kicked ? b.kick : "peer still connected");

    Bot again;
    bot_init(&again, protocol, name_a);
    error[0] = '\0';
    bool reconnected = bot_connect(&again, host, port, error, sizeof error)
        && run_until(&again, NULL, 60000.0, joined_with_chunks, error, sizeof error);
    snprintf(detail, sizeof detail, "joined=%d chunks=%d err=%s", again.joined, again.chunks,
        again.kicked ? again.kick : error);
    step(protocol, "reconnect", reconnected, detail);

    if (again.client != NULL) {
        mc_client_disconnect(again.client);
        mc_client_destroy(again.client);
    }
    if (b.client != NULL) {
        mc_client_disconnect(b.client);
        mc_client_destroy(b.client);
    }
    printf("RESULT %d %s\n", protocol, failures == 0 ? "PASS" : "FAIL");
    return failures == 0 ? 0 : 1;
}
