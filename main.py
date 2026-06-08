import collections
import time
import os
import cv2
import mss
import numpy as np
import psutil
import pygetwindow as gw
import sounddevice as sd
import numpy as np
import collections
from scipy.io import wavfile

DEFAULT_OUTPUT_DIR = "./output"
PROCESS_NAME = "Arknights.exe"
WINDOW_TITLE = "明日方舟"
RECORD_SECONDS = 30           # pre-record duration in seconds
FPS = 30                      # target frames per second for video recording
FRAME_DELAY = 1.0 / FPS
BUFFER_MAX_SIZE = RECORD_SECONDS * FPS
AUDIO_SAMPLE_RATE = 44100     # default sample rate, will be updated to match the device's native rate
AUDIO_CHANNELS = 2

audio_buffer = collections.deque()

# ring buffer to hold the last N frames of the game screen
frame_buffer = collections.deque(maxlen=BUFFER_MAX_SIZE)

def get_game_window_rect():
    # get the window rect of the game, return a dict with top, left, width, height
    try:
        win = gw.getWindowsWithTitle(WINDOW_TITLE)[0]
        if win:
            return {"top": win.top, "left": win.left, "width": win.width, "height": win.height}
    except Exception:
        return None
    return None

def save_buffer_to_video(buffer, filename=f"{DEFAULT_OUTPUT_DIR}/crash_report.mp4"):
    if not buffer:
        return None, None

    timestamps, frames = zip(*buffer) if isinstance(buffer[0], tuple) else (
        [time.time() - len(buffer)/FPS] * len(buffer), 
        list(buffer)
    )
    
    video_start_time = timestamps[0]
    video_end_time = timestamps[-1]
    actual_duration = video_end_time - video_start_time
    
    # fallback
    if actual_duration <= 0:
        actual_duration = len(frames) / FPS
        video_end_time = video_start_time + actual_duration

    first_frame = frames[0]
    height, width, _ = first_frame.shape
    
    fourcc = int(cv2.VideoWriter_fourcc(*'mp4v')) # type: ignore
    out = cv2.VideoWriter(filename, fourcc, FPS, (width, height))
    
    # frame resampling
    total_frames_to_write = max(1, int(actual_duration * FPS))
    print(f"[info] Spanned real time: {actual_duration:.2f}s. Re-sampling and writing {total_frames_to_write} frames to match timeline...")
    
    frame_idx = 0
    num_captured_frames = len(frames)
    
    for i in range(total_frames_to_write):
        current_target_time = video_start_time + (i / FPS)
        
        # resampling alignment
        # find the latest frame whose timestamp is <= current_target_time
        while frame_idx < num_captured_frames - 1 and timestamps[frame_idx + 1] <= current_target_time:
            frame_idx += 1
            
        out.write(frames[frame_idx])
        
    out.release()
    print(f"[info] Video saved to: {filename}")
    
    # absolute timestamps for audio-video synchronization
    return video_start_time, video_end_time

def _start_silence_keeper():
    """
    Silent keep-alive stream: continuously plays all-zero silent data,
    forcing the Windows audio engine to remain active, preventing WASAPI from stopping when the system is muted.
    """
    global AUDIO_SAMPLE_RATE
    
    # Callback function to fill with zeros (silence)
    def silent_callback(outdata, frames, time, status):
        outdata.fill(0.0)
        
    try:
        # create an output stream that continuously plays silence
        silence_stream = sd.OutputStream(
            samplerate=AUDIO_SAMPLE_RATE,
            channels=AUDIO_CHANNELS,
            callback=silent_callback
        )
        silence_stream.start()
        print("[info] Silent keep-alive stream started")
        return silence_stream
    except Exception as e:
        print(f"[warning] Failed to start silent keep-alive stream, audio dropouts may occur: {e}")
        return None

def audio_callback(indata, frames, time_info, status):
    if status:
        print(f"[warning] Audio status warning: {status}")
    # The shape of indata is (frames, channels). Directly extending would flatten it.  
    # To maintain the 2D structure, we need to append frame by frame or as a whole,
    # but the fastest way in deque is to convert it to a list and store it.  
    # Alternatively, to avoid flattening, we can use numpy's append, but deque is more efficient:
    audio_buffer.append((time.time(), indata.copy()))

def start_audio_recording():
    global AUDIO_SAMPLE_RATE
    
    # get the list of audio devices and host APIs to find the correct WASAPI loopback device
    devices = sd.query_devices()
    host_apis = sd.query_hostapis()
    wasapi_api_idx = None
    for i, api in enumerate(host_apis):
        if "WASAPI" in api['name'].upper():
            wasapi_api_idx = i
            break
            
    if wasapi_api_idx is None:
        print("[error] The current system does not support WASAPI, unable to record system audio.")
        return None

    loopback_device_idx = None
    
    # Strictly filter devices that belong to WASAPI and act as "loopback output"
    for idx, d in enumerate(devices):
        if d['hostapi'] == wasapi_api_idx:
            name = d['name'].lower()
            # Key: On Windows, loopback device names usually contain "loopback" or are mirrors of output devices (speakers/headphones)
            if "loopback" in name or "回网" in name or d['max_input_channels'] > 0:
                # Exclude interfering physical microphones
                if "microphone" in name or "麦克风" in name:
                    continue
                
                # Prefer devices with speaker, headphones, or loopback keywords in their names, as they are more likely to be the correct loopback device
                if "speaker" in name or "扬声器" in name or "headphones" in name or "耳机" in name or "loopback" in name:
                    loopback_device_idx = idx
                    AUDIO_SAMPLE_RATE = int(d['default_samplerate'])
                    break

    # If the strict filtering above didn't find a device, relax the criteria to find the first WASAPI input channel (loopback appears as input at the lower level)
    if loopback_device_idx is None:
        for idx, d in enumerate(devices):
            if d['hostapi'] == wasapi_api_idx and d['max_input_channels'] > 0:
                if "microphone" not in d['name'].lower() and "麦克风" not in d['name'].lower():
                    loopback_device_idx = idx
                    AUDIO_SAMPLE_RATE = int(d['default_samplerate'])
                    break

    if loopback_device_idx is None:
        print("[error] Failed to capture WASAPI loopback device, please ensure the system audio driver is functioning correctly.")
        return None

    print(f"[info] Locked system audio loopback device ID [{loopback_device_idx}]: {devices[loopback_device_idx]['name']}")
    print(f"[info] Audio sample rate synchronized to device native: {AUDIO_SAMPLE_RATE}Hz")

    # Start audio stream
    try:
        global AUDIO_BUFFER_MAX, audio_buffer
        AUDIO_BUFFER_MAX = RECORD_SECONDS * AUDIO_SAMPLE_RATE
        audio_buffer = collections.deque(maxlen=AUDIO_BUFFER_MAX)
        
        stream = sd.InputStream(
            samplerate=AUDIO_SAMPLE_RATE,
            channels=AUDIO_CHANNELS,
            callback=audio_callback,
            device=loopback_device_idx
        )
        stream.start()
        return stream
    except Exception as e:
        print(f"[critical] Although the device was found, failed to start audio stream: {e}")
        return None

def save_audio_buffer(filename=f"{DEFAULT_OUTPUT_DIR}/crash_audio.wav", 
                      video_start_time=None,
                      video_end_time=None):
    if not audio_buffer or len(audio_buffer) == 0:
        print("[warning] Audio buffer is empty, no audio file generated.")
        return
    
    # fallback：如果没传入视频时间戳，就用老方法（不推荐）
    if video_start_time is None or video_end_time is None:
        video_start_time = time.time() - RECORD_SECONDS
        video_end_time = time.time()
    
    actual_duration = video_end_time - video_start_time
    total_samples = int(actual_duration * AUDIO_SAMPLE_RATE)
    final_audio = np.zeros((total_samples, AUDIO_CHANNELS), dtype=np.float32)
    
    # 第一步：筛选出与视频窗口有重叠的音频块
    relevant_chunks = []
    for chunk_time, data in audio_buffer:
        chunk_len = len(data)
        chunk_start_time = chunk_time - (chunk_len / AUDIO_SAMPLE_RATE)
        chunk_end_time = chunk_time
        
        # 只保留与视频时间窗口有交集的块
        if chunk_end_time < video_start_time or chunk_start_time > video_end_time:
            continue
        relevant_chunks.append((chunk_start_time, chunk_end_time, chunk_time, data))
    
    if not relevant_chunks:
        print("[warning] No audio chunks overlap with video window.")
        wavfile.write(filename, AUDIO_SAMPLE_RATE, final_audio)
        return
    
    # 第二步：在相关块内部做连续拼接（保留原始抗抖动逻辑）
    last_end_sample = 0
    last_chunk_end_time = 0
    
    for chunk_start_time, chunk_end_time, chunk_time, data in relevant_chunks:
        chunk_len = len(data)
        
        # 计算这块音频在最终文件中的理论起始位置
        theoretical_start = int((chunk_start_time - video_start_time) * AUDIO_SAMPLE_RATE)
        
        if last_chunk_end_time == 0 or (chunk_start_time - last_chunk_end_time) > 0.1:
            # 明显不连续（>100ms），按绝对时间戳放置，可能是游戏启动前的静音或真正的间隙
            start_sample = max(0, theoretical_start)
        else:
            # 连续播放的音频，紧跟前一块结束，消除微间隙/重叠导致的高频爆音
            start_sample = last_end_sample
        
        end_sample = start_sample + chunk_len
        
        # 边界裁剪，防止越界
        if start_sample >= total_samples:
            break
        if end_sample > total_samples:
            data = data[:total_samples - start_sample]
            end_sample = total_samples
        
        if start_sample < 0:
            # 这块音频开始于视频窗口之前，截掉前面多余部分
            skip_samples = -start_sample
            data = data[skip_samples:]
            start_sample = 0
        
        # 写入
        write_len = len(data)
        if write_len > 0:
            final_audio[start_sample:start_sample + write_len] = data[:write_len]
        
        last_end_sample = start_sample + write_len
        last_chunk_end_time = chunk_end_time  # 记录这块的结束时间，用于下一块连续性判断
    
    # 可选：对首尾做极短的淡入淡出，进一步消除边界高频噪声
    fade_samples = min(256, total_samples // 2)
    if fade_samples > 0:
        fade_in = np.linspace(0.0, 1.0, fade_samples).reshape(-1, 1)
        fade_out = np.linspace(1.0, 0.0, fade_samples).reshape(-1, 1)
        final_audio[:fade_samples] *= fade_in
        final_audio[-fade_samples:] *= fade_out
    
    wavfile.write(filename, AUDIO_SAMPLE_RATE, final_audio)
    print(f"[info] WAV saved: {filename}, duration: {actual_duration:.2f}s, "
          f"aligned to video [{video_start_time:.3f} ~ {video_end_time:.3f}]")

def try_merge_audio_video(video_file=f"{DEFAULT_OUTPUT_DIR}/crash_report.mp4", audio_file=f"{DEFAULT_OUTPUT_DIR}/crash_audio.wav", output_file=f"{DEFAULT_OUTPUT_DIR}/final_report.mp4"):
    # This function attempts to merge audio and video using ffmpeg
    # Requires ffmpeg to be installed and accessible in the system PATH
    if not os.path.exists(video_file) or not os.path.exists(audio_file):
        print("[error] Cannot merge: missing video or audio file")
        return
    
    command = f'ffmpeg -y -i "{video_file}" -i "{audio_file}" -c:v copy -c:a aac "{output_file}"'
    print("[info] Merging audio and video...")
    result = os.system(command)
    if result == 0:
        print(f"[info] Merge completed, final report saved to: {output_file}")
    else:
        print("[error] Merge failed, please ensure ffmpeg is installed and in your PATH.")

def main():
    sct = mss.mss()
    audio_stream = start_audio_recording()
    print("[info] Pre-recording started, waiting for the game to launch...")
    game_started: bool = False
    
    while True:
        # check if the game process exists and get the window rect
        process_exists = any(p.name() == PROCESS_NAME for p in psutil.process_iter(attrs=['name']))
        
        rect = get_game_window_rect()
        now = time.time()
        while audio_buffer and audio_buffer[0][0] < now - RECORD_SECONDS - 2: # 多留2秒容错
            audio_buffer.popleft()
        
        if process_exists and rect:
            start_time = time.time()
            if not game_started:
                print("[info] Arknights process detected and window found, starting pre-recording...")
                game_started = True
                video_start_timestamp = time.time()
            
            # if gw.getActiveWindow().title != "明日方舟": continue
            
            # capture the screen
            # mss would capture the screen in BGRA format, we need to convert it to BGR for OpenCV
            sct_img = sct.grab(rect)
            frame = np.array(sct_img)
            frame = cv2.cvtColor(frame, cv2.COLOR_BGRA2BGR) # 转换为 OpenCV 格式
            
            # push to buffer
            frame_buffer.append((time.time(), frame))
            
            # frame rate control
            elapsed = time.time() - start_time
            if elapsed < FRAME_DELAY:
                time.sleep(FRAME_DELAY - elapsed)
                
        elif len(frame_buffer) > 0:
            # save the crash report when the game process is lost
            print("[info] Game process lost, saving crash report...")
            
            video_start, video_end = save_buffer_to_video(frame_buffer)
            
            if video_start and video_end:
                save_audio_buffer(video_start_time=video_start, video_end_time=video_end)
            else:
                print("[warning] Missing video timestamps, audio may not be properly synchronized.")
                save_audio_buffer()
            try_merge_audio_video()
            break
        else:
            time.sleep(1)

if __name__ == "__main__":
    main()