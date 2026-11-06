// The comux host's C ABI (comux-host/src/ffi.rs, decisions #166/#169). copad-macos declares the
// same functions in CCopadFFI/include/copad_ffi.h; AIDE copies this file at the rev it pins.
// Strings returned are freed with comux_host_free_string (copad also uses copad_ffi_free_string).

#ifndef COMUX_HOST_H
#define COMUX_HOST_H

#include <stdint.h>

void comux_host_free_string(char *s);

typedef struct MuxHostHandle MuxHostHandle;
/// dest NULL/"" = the local comux socket; else `ssh -T <dest> comux proxy --stdio`.
MuxHostHandle *copad_muxhost_start(const char *dest);
void copad_muxhost_destroy(MuxHostHandle *h);
unsigned long long copad_muxhost_generation(const MuxHostHandle *h);
void copad_muxhost_bind(const MuxHostHandle *h, const char *panel, const char *tag);
void copad_muxhost_unbind(const MuxHostHandle *h, const char *panel);
/// JSON array of the panel ids on screen.
void copad_muxhost_serve(const MuxHostHandle *h, const char *panels_json);
/// Free with comux_host_free_string.
char *copad_muxhost_state_json(const MuxHostHandle *h);
void copad_muxhost_command(const MuxHostHandle *h, const char *req_json);
/// Ask comux to open one of its overlays for a panel's client (decisions #172); target NULL = active.
void copad_muxhost_open(const MuxHostHandle *h, const char *panel, const char *what, const char *target);
/// Remote comux (decisions #171). NULL when dest/comux are refused.
MuxHostHandle *copad_muxhost_start_remote(const char *dest, const char *comux);
/// Free with comux_host_free_string.
char *copad_muxhost_new_remote_tag(void);
/// The command a tab runs to attach the remote comux; NULL when refused. Free with comux_host_free_string.
char *copad_muxhost_remote_attach_command(const char *dest, const char *comux, const char *tag);
/// Answer a delegated overlay; choice/text NULL when absent (both NULL = cancel).
void copad_muxhost_answer(const MuxHostHandle *h, unsigned long long overlay, const char *choice, const char *text);
/// Answer with a staying choice (the overlay stays until comux re-sends it).
void copad_muxhost_act(const MuxHostHandle *h, unsigned long long overlay, const char *choice);
/// Whether panel's comux client asked to focus the native sidebar (Ctrl-b e); 1 once per request, else 0.
int copad_muxhost_take_focus(const MuxHostHandle *h, const char *panel);
/// Keep consumer's preview of pane (rows x cols) fresh; pane NULL stops it (decisions #180).
void copad_muxhost_preview(const MuxHostHandle *h, const char *consumer, const char *pane, unsigned short rows, unsigned short cols);
/// Free with comux_host_free_string.
char *copad_muxhost_copad_tag(const char *panel, const char *socket);
/// The work graph (decisions #204): {rev, epoch, data: {graph, agents}, page, page_seq} for consumer.
/// Free with comux_host_free_string.
char *copad_muxhost_graph_json(const MuxHostHandle *h, const char *consumer);
/// Ask for a page of token's history: before < 0 = the newest, limit <= 0 = comux's default.
void copad_muxhost_graph_history(const MuxHostHandle *h, const char *consumer, const char *token, long long before, long long limit);
/// The work-graph page as one HTML document. Free with comux_host_free_string.
char *copad_graph_page(void);

#endif // COMUX_HOST_H
