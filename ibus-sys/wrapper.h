#ifndef IBUS_DIKT_WRAPPER_H
#define IBUS_DIKT_WRAPPER_H

#include <ibus.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef gboolean (*ibus_dikt_callback_key_event)(void* ctx, IBusEngine* engine, guint keyval, guint keycode, guint modifiers);
typedef void (*ibus_dikt_callback_focus_in)(void* ctx, IBusEngine* engine);
typedef void (*ibus_dikt_callback_focus_out)(void* ctx, IBusEngine* engine);
typedef void (*ibus_dikt_callback_reset)(void* ctx, IBusEngine* engine);
typedef void (*ibus_dikt_callback_enable)(void* ctx, IBusEngine* engine);
typedef void (*ibus_dikt_callback_disable)(void* ctx, IBusEngine* engine);

void ibus_dikt_set_callback(
    void* ctx,
    ibus_dikt_callback_key_event key_event_cb,
    ibus_dikt_callback_focus_in focus_in_cb,
    ibus_dikt_callback_focus_out focus_out_cb,
    ibus_dikt_callback_reset reset_cb,
    ibus_dikt_callback_enable enable_cb,
    ibus_dikt_callback_disable disable_cb
);

int ibus_dikt_init(bool ibus_mode);
void ibus_dikt_cleanup(void);
gboolean ibus_dikt_set_global_engine(const gchar* engine_name);
gchar* ibus_dikt_get_global_engine_name(void);
gboolean ibus_dikt_daemon_set_global_engine(const gchar* engine_name);
gchar* ibus_dikt_daemon_get_global_engine_name(void);

typedef struct {
    IBusEngine parent;
} IBusDiktEngine;

#ifdef __cplusplus
}
#endif

#endif
