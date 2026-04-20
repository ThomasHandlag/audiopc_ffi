export 'package:audiopc_interface/audiopc_interface.dart';

import 'dart:async';
import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:typed_data';

import 'package:audiopc_interface/audiopc_interface.dart';
import 'package:ffi/ffi.dart';

import 'audiopc_ffi.g.dart' as bindings;

/// Native player implementation backed by Rust FFI.
class AudiopcNative with PlayerStateMixin implements AudiopcInterface {
  static bool _ok(int code) => code == 0;

  late final Timer _positionTimer;
  late final Timer _stateTimer;

  /// Reads backend capabilities from the Rust/CPAL layer.
  @override
  AudioBackendInfo getAudioBackendInfo() {
    return AudioBackendInfo(
      defaultOutputSampleRate: bindings.audiopc_default_output_sample_rate(),
      defaultOutputChannels: bindings.audiopc_default_output_channels(),
      outputDeviceCount: bindings.audiopc_output_device_count(),
    );
  }

  /// Creates a player and starts a periodic position publisher.
  AudiopcNative() {
    _positionTimer = Timer.periodic(const Duration(milliseconds: 100), (_) {
      if (state == PlayerState.playing) {
        positionController.add(positionMillis);
      }
    });

    _stateTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      // Polling for state changes is not ideal, but the Rust backend does not currently support callbacks.
      // In a future iteration, we could add a callback mechanism to notify Dart of state changes immediately.
      final stateCode = bindings.audiopc_get_player_state();
      if (stateCode >= 0 && stateCode < PlayerState.values.length) {
        setState(PlayerState.values[stateCode]);
      }
    });
  }

  /// Sets a local file path as the active source.
  @override
  bool setFileSource(String path) {
    final ptr = path.toNativeUtf8().cast<ffi.Char>();
    try {
      setState(PlayerState.idle);
      return _ok(bindings.audiopc_set_source_path(ptr));
    } finally {
      calloc.free(ptr);
    }
  }

  /// Sets a direct URL as the active source.
  @override
  bool setUrlSource(String url) {
    final ptr = url.toNativeUtf8().cast<ffi.Char>();
    try {
      setState(PlayerState.idle);
      return _ok(bindings.audiopc_set_source_url(ptr));
    } finally {
      calloc.free(ptr);
    }
  }

  /// Sets an in-memory byte buffer as the active source.
  @override
  bool setMemorySource(List<int> data) {
    final ptr = malloc.allocate<ffi.Uint8>(data.length);
    try {
      final byteList = ptr.asTypedList(data.length);
      byteList.setAll(0, data);
      return _ok(bindings.audiopc_set_source_memory(ptr, data.length));
    } finally {
      malloc.free(ptr);
    }
  }

  /// Seeks to a playback position in milliseconds.
  @override
  void seek(int positionMillis) => bindings.audiopc_seek_millis(positionMillis);

  /// Starts or resumes playback.
  @override
  bool play() {
    final isOk = _ok(bindings.audiopc_play());
    if (isOk) {
      setState(PlayerState.playing);
    }
    return isOk;
  }

  /// Pauses active playback.
  @override
  bool pause() {
    final isOk = _ok(bindings.audiopc_pause());
    if (isOk) {
      setState(PlayerState.paused);
    }
    return isOk;
  }

  /// Stops playback and resets to the idle state.
  @override
  bool stop() {
    final isOk = _ok(bindings.audiopc_stop());
    if (isOk) {
      setState(PlayerState.stopped);
    }
    return isOk;
  }

  /// Convenience method to play a source directly from a URL or file path.
  void playSource(String source) {
    if (source.startsWith('http://') || source.startsWith('https://')) {
      setUrlSource(source);
    } else {
      setFileSource(source);
    }
    play();
  }

  /// Convenience method to play directly from an in-memory byte buffer.
  void playMemory(Uint8List data) {
    setMemorySource(data);
    play();
  }

  /// Sets output gain where 1.0 is the nominal level.
  @override
  bool setVolume(double value) => _ok(bindings.audiopc_set_volume(value));

  /// Sets low-pass cutoff in Hz. Use 0 to disable filtering.
  @override
  bool setLowPassHz(double hz) => _ok(bindings.audiopc_set_lowpass_hz(hz));

  /// Number of decoded samples waiting in the native buffer.
  @override
  int get bufferedSamples => bindings.audiopc_buffered_samples();

  /// Current playback position in milliseconds.
  @override
  int get positionMillis => bindings.audiopc_position_millis();

  /// Total media duration in milliseconds, or a negative value if unknown.
  @override
  int get durationMillis => bindings.audiopc_duration_millis();

  /// Number of visualizer samples ready to be copied.
  @override
  int get visualizerAvailableSamples =>
      bindings.audiopc_visualizer_available_samples();

  /// Visualizer sample rate reported by the backend.
  @override
  int get visualizerSampleRate => bindings.audiopc_visualizer_sample_rate();

  /// Visualizer channel count reported by the backend.
  @override
  int get visualizerChannels => bindings.audiopc_visualizer_channels();

  /// Copies normalized time-domain visualizer samples.
  @override
  List<double> getVisualizerSamples(int maxSamples) {
    if (maxSamples <= 0) {
      return const [];
    }

    final ptr = calloc<ffi.Float>(maxSamples);
    try {
      final copied = bindings.audiopc_copy_visualizer_samples(ptr, maxSamples);
      if (copied <= 0) {
        return const [];
      }
      final raw = ptr.asTypedList(copied);
      return List<double>.generate(
        copied,
        (index) => raw[index].toDouble(),
        growable: false,
      );
    } finally {
      calloc.free(ptr);
    }
  }

  /// Copies normalized frequency-domain bars for a spectrum view.
  @override
  List<double> getVisualizerSpectrum(int maxBars) {
    if (maxBars <= 0) {
      return const [];
    }

    final ptr = calloc<ffi.Float>(maxBars);
    try {
      final copied = bindings.audiopc_copy_visualizer_spectrum(ptr, maxBars);
      if (copied <= 0) {
        return const [];
      }

      final raw = ptr.asTypedList(copied);
      return List<double>.generate(
        copied,
        (index) => raw[index].toDouble(),
        growable: false,
      );
    } finally {
      calloc.free(ptr);
    }
  }

  /// Retrieves the current metadata snapshot from the native backend.
  @override
  MetaData getMetadata(String url) {
    const maxLen = 8192; // Must match Rust buffer size
    final ptr = calloc<ffi.Char>(maxLen);
    final urlPtr = url.toNativeUtf8();
    try {
      if (ptr == ffi.nullptr) {
        throw Exception('Failed to retrieve metadata');
      }
      final result = bindings.audiopc_get_metadata(ptr, maxLen, urlPtr.cast());
      if (result < 0) {
        throw Exception('Failed to retrieve metadata (error code: $result)');
      }
      final jsonString = ptr.cast<Utf8>().toDartString();
      final jsonData = jsonDecode(jsonString) as Map<String, dynamic>;

      return MetaData.fromJson(jsonData);
    } finally {
      calloc.free(ptr);
      calloc.free(urlPtr);
    }
  }

  /// Retrieves the thumbnail image data from the native backend.
  @override
  Uint8List? getThumbnail(String url, {int maxSizeBytes = 20 * 1024 * 1024}) {
    final urlPtr = url.toNativeUtf8().cast<ffi.Char>();
    try {
      // First, query size (requires API change to support this)
      // For now, allocate a reasonable max size
      const maxThumbnailSize = 20 * 1024 * 1024; // 20MB max
      final ptr = calloc<ffi.Uint8>(maxThumbnailSize);
      if (ptr == ffi.nullptr) {
        throw Exception('Failed to allocate thumbnail buffer');
      }
      try {
        final length = bindings.audiopc_get_thumbnail(
          ptr,
          maxThumbnailSize,
          urlPtr,
        );
        if (length <= 0) {
          return null;
        }
        // Copy data before freeing
        final result = Uint8List(length);
        result.setAll(0, ptr.asTypedList(length));
        return result;
      } finally {
        calloc.free(ptr);
      }
    } finally {
      calloc.free(urlPtr);
    }
  }  
  
  /// Stops playback and releases timers and stream controllers.
  @override
  void dispose() {
    stop();
    _positionTimer.cancel();
    _stateTimer.cancel();
    positionController.close();
    playerStateController.close();
  }

  @override
  Stream<int> get positionStream => positionController.stream;

  @override
  Stream<PlayerState> get stateStream => playerStateController.stream;
}
