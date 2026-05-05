package com.thugbn.audiopc_ffi;

import android.content.Context;

public final class AudiopcBridge {
    static {
        System.loadLibrary("audiopc_ffi");
    }

    private AudiopcBridge() {}

    public static void init(Context context) {
        nativeInit(context.getApplicationContext());
    }

    private static native void nativeInit(Context context);
}
