/* The procd.h this build compiles against, as src/procd.rs relies on it.
 *
 * Its function signatures are checked here, at compile time: each is
 * assigned to a pointer of the type src/procd.rs declares, which does not
 * compile when the header's differs (build.rs turns warnings into errors).
 * Its layouts and constants are reported by agentctl_procd_fact() for
 * procd::tests to compare with the Rust mirror. Nothing here is a version
 * or content check: whatever procd.h says is the truth for this build. */

#include <stddef.h>
#include <string.h>

#include "procd.h"

const struct {
    procd_status (*capabilities_probe)(procd_capabilities *);
    procd_status (*create_domain)(const procd_policy *, procd_domain **);
    procd_status (*domain_spawn)(procd_domain *, const char *const *, int64_t *);
    procd_status (*domain_status_get)(procd_domain *, procd_domain_status *);
    procd_status (*domain_terminate)(procd_domain *, int, procd_termination_evidence *);
    procd_status (*domain_identity)(procd_domain *, char *, size_t);
    procd_status (*domain_release)(procd_domain *);
    procd_status (*recover)(const char *, procd_recovery_outcome *, procd_domain **);
    const char *(*status_name)(procd_status);
} agentctl_procd_signatures = {
    procd_capabilities_probe, procd_create_domain,  procd_domain_spawn,
    procd_domain_status_get,  procd_domain_terminate, procd_domain_identity,
    procd_domain_release,     procd_recover,        procd_status_name,
};

#define VALUE(name, value) {name, (long long)(value)}
#define TYPE(T) VALUE("sizeof " #T, sizeof(T)), VALUE("alignof " #T, _Alignof(T))
#define FIELD(T, f)                                                                                \
    VALUE(#T "." #f " offset", offsetof(T, f)), VALUE(#T "." #f " size", sizeof(((T *)0)->f))
#define CONSTANT(c) VALUE(#c, c)

static const struct {
    const char *name;
    long long value;
} facts[] = {
    /* src/procd.rs passes and reads every enum as a C int. */
    TYPE(procd_status),
    TYPE(procd_capability),
    TYPE(procd_crash_behavior),
    TYPE(procd_enforcement),
    TYPE(procd_lifecycle_state),
    TYPE(procd_population),
    TYPE(procd_recovery_outcome),

    TYPE(procd_policy),
    FIELD(procd_policy, enforcement),
    FIELD(procd_policy, label),
    FIELD(procd_policy, drop_uid),
    FIELD(procd_policy, drop_gid),

    TYPE(procd_capabilities),
    FIELD(procd_capabilities, process_tree_termination),
    FIELD(procd_capabilities, pre_execution_containment),
    FIELD(procd_capabilities, descendant_containment),
    FIELD(procd_capabilities, topology_escape_resistance),
    FIELD(procd_capabilities, domain_emptiness_proof),
    FIELD(procd_capabilities, safe_recovery),
    FIELD(procd_capabilities, crash_behavior),
    FIELD(procd_capabilities, backend),
    FIELD(procd_capabilities, detail),

    TYPE(procd_domain_status),
    FIELD(procd_domain_status, state),
    FIELD(procd_domain_status, population),
    FIELD(procd_domain_status, process_tree_termination),
    FIELD(procd_domain_status, population_is_authoritative),

    TYPE(procd_termination_evidence),
    FIELD(procd_termination_evidence, admission_closed),
    FIELD(procd_termination_evidence, authority_directed),
    FIELD(procd_termination_evidence, emptiness_proven),
    FIELD(procd_termination_evidence, enforced),
    FIELD(procd_termination_evidence, final_state),
    FIELD(procd_termination_evidence, detail),

    CONSTANT(PROCD_OK),
    CONSTANT(PROCD_E_UNSUPPORTED_ENFORCEMENT),
    CONSTANT(PROCD_E_PREREQUISITE),
    CONSTANT(PROCD_REQUIRE_ENFORCED),
    CONSTANT(PROCD_ALLOW_BEST_EFFORT),
    CONSTANT(PROCD_IDENTITY_MAX),
    CONSTANT(PROCD_CAP_UNSUPPORTED),
    CONSTANT(PROCD_CAP_BEST_EFFORT),
    CONSTANT(PROCD_CAP_ENFORCED),
    CONSTANT(PROCD_CRASH_AUTOMATIC_DESTRUCTION),
    CONSTANT(PROCD_CRASH_DURABLE_REACQUISITION),
    CONSTANT(PROCD_CRASH_UNRESOLVED_ON_AUTHORITY_LOSS),
    CONSTANT(PROCD_STATE_CREATED),
    CONSTANT(PROCD_STATE_ACTIVE),
    CONSTANT(PROCD_STATE_TERMINATING),
    CONSTANT(PROCD_STATE_EMPTY),
    CONSTANT(PROCD_STATE_RELEASED),
    CONSTANT(PROCD_STATE_UNRESOLVED),
    CONSTANT(PROCD_RECOVERED),
    CONSTANT(PROCD_CONFIRMED_DESTROYED),
    CONSTANT(PROCD_UNRESOLVED),
};

/* Sets *out to the fact called name; 0 when there is none. */
int agentctl_procd_fact(const char *name, long long *out) {
    for (size_t i = 0; i < sizeof facts / sizeof facts[0]; i++) {
        if (strcmp(facts[i].name, name) == 0) {
            *out = facts[i].value;
            return 1;
        }
    }
    return 0;
}
