/*
 * ABI conformance smoke test.
 *
 * Copyright (C) 2026 Javad Rajabzadeh
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * This program does almost nothing, on purpose. It exists to catch the class of
 * problem that Rust unit tests structurally cannot: that the committed header
 * does not compile, that a struct is laid out differently on the two sides of
 * the boundary, that a symbol is missing from the static library, or that the
 * library cannot be linked at all on this platform.
 *
 * It touches no network and creates no files, so it is safe to run anywhere,
 * including in a sandboxed CI job.
 */
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "hydra.h"

/* --------------------------------------------------------------------------
 * Layout, checked at compile time on both sides of the boundary.
 *
 * The matching table lives in `crates/hydra-ffi/src/abi.rs`. Duplication is the
 * point: the Rust assertions prove the Rust structs did not move, and these
 * prove the C compiler agrees about the same numbers. A padding rule, an enum
 * width or a `size_t` that differed between the two would be invisible to
 * either table alone, and would show up at run time as a caller reading the
 * wrong bytes out of a struct that compiled and linked perfectly.
 *
 * 64-bit only: pointers and size_t change width, so one table cannot describe
 * both, and every supported platform is 64-bit today.
 * -------------------------------------------------------------------------- */
#if defined(__SIZEOF_POINTER__) && __SIZEOF_POINTER__ == 8
#define HYDRA_ASSERT_LAYOUT(type, bytes)                                       \
    _Static_assert(sizeof(type) == (bytes), #type " has the wrong size: ABI break")

HYDRA_ASSERT_LAYOUT(hydra_string_t, 16);
HYDRA_ASSERT_LAYOUT(hydra_error_t, 32);
HYDRA_ASSERT_LAYOUT(hydra_header_t, 16);
HYDRA_ASSERT_LAYOUT(hydra_proxy_config_t, 32);
HYDRA_ASSERT_LAYOUT(hydra_checksum_t, 24);
HYDRA_ASSERT_LAYOUT(hydra_runtime_policy_t, 16);
HYDRA_ASSERT_LAYOUT(hydra_engine_config_t, 104);
HYDRA_ASSERT_LAYOUT(hydra_job_config_t, 160);
HYDRA_ASSERT_LAYOUT(hydra_progress_t, 72);
HYDRA_ASSERT_LAYOUT(hydra_event_t, 120);
HYDRA_ASSERT_LAYOUT(hydra_job_snapshot_t, 176);
HYDRA_ASSERT_LAYOUT(hydra_source_info_t, 64);
HYDRA_ASSERT_LAYOUT(hydra_source_array_t, 16);
HYDRA_ASSERT_LAYOUT(hydra_job_id_array_t, 16);
HYDRA_ASSERT_LAYOUT(hydra_metrics_t, 80);

/* Every enum is 32 bits wide, in C and in C++ alike. */
_Static_assert(sizeof(hydra_error_code_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_job_state_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_event_type_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_cancel_mode_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_priority_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_network_policy_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_power_mode_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_proxy_type_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_checksum_algorithm_t) == 4, "enum width changed");
_Static_assert(sizeof(hydra_log_level_t) == 4, "enum width changed");

/* Offsets, which catch the reordering that sizes alone would not. */
_Static_assert(offsetof(hydra_error_t, message) == 16, "field moved: ABI break");
_Static_assert(offsetof(hydra_progress_t, total_bytes) == 8, "field moved: ABI break");
_Static_assert(offsetof(hydra_progress_t, retry_count) == 56, "field moved: ABI break");
_Static_assert(offsetof(hydra_event_t, progress) == 16, "field moved: ABI break");
_Static_assert(offsetof(hydra_event_t, timestamp_ms) == 104, "field moved: ABI break");
_Static_assert(offsetof(hydra_engine_config_t, max_bytes_per_second) == 32, "field moved: ABI break");
_Static_assert(offsetof(hydra_engine_config_t, state_path) == 56, "field moved: ABI break");
_Static_assert(offsetof(hydra_engine_config_t, reserved) == 72, "field moved: ABI break");
_Static_assert(offsetof(hydra_job_config_t, checksum) == 72, "field moved: ABI break");
_Static_assert(offsetof(hydra_job_config_t, reserved) == 124, "field moved: ABI break");
_Static_assert(offsetof(hydra_job_snapshot_t, url) == 88, "field moved: ABI break");
#endif

static int failures = 0;

#define CHECK(cond, ...)                                                       \
    do {                                                                       \
        if (!(cond)) {                                                         \
            fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__);               \
            fprintf(stderr, __VA_ARGS__);                                      \
            fputc('\n', stderr);                                               \
            failures++;                                                        \
        }                                                                      \
    } while (0)

int main(void)
{
    /* The header and the library must agree about the ABI before anything else
     * in this program means anything. */
    CHECK(hydra_ffi_abi_version() == HYDRA_FFI_ABI_VERSION,
          "header says ABI %u, library says %u", (unsigned)HYDRA_FFI_ABI_VERSION,
          hydra_ffi_abi_version());
    CHECK(strcmp(hydra_ffi_version_string(), HYDRA_FFI_VERSION) == 0,
          "header says version %s, library says %s", HYDRA_FFI_VERSION,
          hydra_ffi_version_string());
    CHECK(HYDRA_FFI_VERSION_NUMBER >= 0, "version number macro is unusable");
    CHECK(strcmp(hydra_error_name(HYDRA_OK), "HYDRA_OK") == 0,
          "error names are wrong");
    /* HYDRA_ERR_AGAIN is "nothing to report", not a failure. A binding that got
     * this wrong would treat an empty event queue as an error every poll. */
    CHECK(HYDRA_SUCCEEDED(HYDRA_OK) && !HYDRA_FAILED(HYDRA_OK), "OK classifiers");
    CHECK(HYDRA_FAILED(HYDRA_ERR_AGAIN), "AGAIN is not success");
    CHECK(!HYDRA_IS_ERROR(HYDRA_ERR_AGAIN), "AGAIN must not read as an error");
    CHECK(HYDRA_IS_ERROR(HYDRA_ERR_IO), "a real failure must read as an error");

    /* Struct layout: if the two sides disagree, `size` will not survive the
     * round trip through the library's own validation. */
    hydra_engine_config_t cfg;
    memset(&cfg, 0, sizeof cfg);
    CHECK(HYDRA_ENGINE_CONFIG_INIT(&cfg) == HYDRA_OK, "engine config init failed");
    CHECK(cfg.size == (uint32_t)sizeof cfg, "config size was not stamped");
    CHECK(cfg.version == HYDRA_ENGINE_CONFIG_VERSION, "config version was not stamped");
    CHECK(cfg.max_jobs > 0 && cfg.max_connections > 0, "defaults are not sane");

    hydra_job_config_t job;
    memset(&job, 0, sizeof job);
    CHECK(HYDRA_JOB_CONFIG_INIT(&job) == HYDRA_OK, "job config init failed");
    CHECK(job.size == (uint32_t)sizeof job, "job config size was not stamped");
    CHECK(job.resume == 1, "resume should default on");
    CHECK(job.auto_start == 0, "creating a job must not start it");

    hydra_runtime_policy_t policy;
    memset(&policy, 0, sizeof policy);
    CHECK(hydra_runtime_policy_init(&policy) == HYDRA_OK, "policy init failed");

    /* --------------------------------------------------------------------
     * The forward-compatibility promise, tested rather than documented.
     *
     * This is what a program built against an OLDER header looks like: a
     * struct that stops after the fields that existed then. The library must
     * read exactly `size` bytes, use what it finds, and default everything
     * past it — it must NOT read the fields this struct does not have, which
     * would be reading our stack.
     *
     * A library that cast the pointer to its own full struct and read normally
     * would pass every other check in this file and fail here.
     * ------------------------------------------------------------------ */
    struct hydra_engine_config_old {
        uint32_t size;
        uint32_t version;
        uint32_t max_jobs;
        uint32_t max_connections;
    };
    /* The prefix must actually be a prefix, or the test proves nothing. */
    _Static_assert(offsetof(struct hydra_engine_config_old, max_jobs)
                       == offsetof(hydra_engine_config_t, max_jobs),
                   "the old layout is not a prefix of the current one");
    _Static_assert(offsetof(struct hydra_engine_config_old, max_connections)
                       == offsetof(hydra_engine_config_t, max_connections),
                   "the old layout is not a prefix of the current one");
    _Static_assert(sizeof(struct hydra_engine_config_old) < sizeof(hydra_engine_config_t),
                   "the old layout must be smaller, or this proves nothing");

    struct hydra_engine_config_old old;
    memset(&old, 0, sizeof old);
    CHECK(hydra_engine_config_init((hydra_engine_config_t *)&old,
                                   (uint32_t)sizeof old) == HYDRA_OK,
          "init refused a smaller (older) struct");
    CHECK(old.size == (uint32_t)sizeof old, "init stamped the wrong size");
    CHECK(old.version == HYDRA_ENGINE_CONFIG_VERSION, "init stamped the wrong version");
    CHECK(old.max_jobs > 0 && old.max_connections > 0,
          "init did not fill the fields the old struct does have");

    old.max_jobs = 3;
    old.max_connections = 2;
    hydra_engine_t *from_old = hydra_engine_create((const hydra_engine_config_t *)&old);
    CHECK(from_old != NULL, "the library refused a valid older configuration");
    if (from_old) {
        hydra_engine_destroy(from_old);
    }

    /* The other direction: a program built against a NEWER header, whose
     * struct carries fields appended past what this library knows. Init must
     * zero them - the value every appended field defaults from - rather than
     * leave whatever the caller's stack held. */
    {
        union {
            uint64_t align;
            unsigned char bytes[sizeof(hydra_engine_config_t) + 64];
        } newer;
        size_t i;
        memset(newer.bytes, 0xFF, sizeof newer.bytes);
        CHECK(hydra_engine_config_init((hydra_engine_config_t *)newer.bytes,
                                       (uint32_t)sizeof newer.bytes) == HYDRA_OK,
              "init refused a larger (newer) struct");
        CHECK(((hydra_engine_config_t *)newer.bytes)->size == (uint32_t)sizeof newer.bytes,
              "init stamped this build's size rather than the caller's");
        for (i = sizeof(hydra_engine_config_t); i < sizeof newer.bytes; i++) {
            if (newer.bytes[i] != 0) {
                CHECK(0, "byte %zu past this build's struct was not zeroed", i);
                break;
            }
        }
        /* And a size no configuration will ever reach is refused outright. */
        CHECK(hydra_engine_config_init(&cfg, 1u << 20) == HYDRA_ERR_INVALID_ARGUMENT,
              "an absurd struct_size must be refused");
    }

    /* The one call every embedder makes first and last. */
    hydra_engine_t *engine = hydra_engine_create(&cfg);
    CHECK(engine != NULL, "engine creation returned NULL");
    if (engine) {
        hydra_metrics_t m;
        memset(&m, 0, sizeof m);
        CHECK(hydra_engine_get_metrics(engine, &m) == HYDRA_OK, "metrics failed");
        CHECK(m.jobs_created == 0, "a fresh engine has no jobs");

        hydra_job_id_array_t ids;
        memset(&ids, 0, sizeof ids);
        CHECK(hydra_engine_list_jobs(engine, &ids) == HYDRA_OK, "list failed");
        CHECK(ids.len == 0, "a fresh engine lists no jobs");
        hydra_job_id_array_free(&ids);

        /* An empty queue reports "nothing yet", not a failure. */
        hydra_event_t ev;
        memset(&ev, 0, sizeof ev);
        CHECK(hydra_event_next(engine, &ev) == HYDRA_ERR_AGAIN,
              "an empty queue should return HYDRA_ERR_AGAIN");

        /* Refusals must be refusals, not crashes. */
        CHECK(hydra_job_start(engine, 999) == HYDRA_ERR_NOT_FOUND,
              "an unknown job id should be NOT_FOUND");
        CHECK(hydra_job_create(engine, NULL, NULL) == HYDRA_ERR_INVALID_ARGUMENT,
              "a NULL job config should be INVALID_ARGUMENT");
        {
            /* The Metalink entry point takes the same struct and must refuse
             * the same NULL rather than read through it. */
            hydra_metalink_t *doc = NULL;
            hydra_job_id_t id = 0;
            CHECK(hydra_metalink_parse(
                      "<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"f\">"
                      "<size>1</size><url>https://example.invalid/f</url></file></metalink>",
                      &doc) == HYDRA_OK,
                  "a minimal document should parse");
            CHECK(hydra_job_create_from_metalink(engine, doc, 0, NULL, &id)
                      == HYDRA_ERR_INVALID_ARGUMENT,
                  "a NULL job config should be INVALID_ARGUMENT on the Metalink path too");
            hydra_metalink_free(doc);
        }
        CHECK(hydra_event_wake(engine) == HYDRA_OK, "wake with nobody waiting is fine");
        CHECK(hydra_engine_set_log_callback(engine, NULL, NULL, HYDRA_LOG_INFO) == HYDRA_OK,
              "clearing the log sink should succeed");

        hydra_error_t err;
        memset(&err, 0, sizeof err);
        if (hydra_last_error(&err) == HYDRA_OK) {
            CHECK(err.message.data != NULL, "a failure should carry a message");
            hydra_error_free(&err);
        }

        CHECK(hydra_engine_shutdown(engine, 1000) == HYDRA_OK, "shutdown failed");
        hydra_engine_destroy(engine);
    }

    /* Every free must tolerate the null value, so a binding's destructor can be
     * unconditional. */
    hydra_string_free((hydra_string_t){ NULL, 0 });
    hydra_error_free(NULL);
    hydra_job_snapshot_free(NULL);
    hydra_source_array_free(NULL);
    hydra_job_id_array_free(NULL);
    hydra_engine_destroy(NULL);

    if (failures) {
        fprintf(stderr, "\n%d check(s) failed\n", failures);
        return 1;
    }
    printf("ABI smoke test passed (libhydra %s, ABI %u)\n",
           hydra_ffi_version_string(), hydra_ffi_abi_version());
    return 0;
}
