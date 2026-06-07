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
AUDIO_BUFFER_MAX = RECORD_SECONDS * AUDIO_SAMPLE_RATE

audio_buffer = collections.deque(maxlen=AUDIO_BUFFER_MAX)

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
        return
    first_frame = buffer[0]
    height, width, _ = first_frame.shape
    
    fourcc = int(cv2.VideoWriter_fourcc(*'mp4v')) # type: ignore
    out = cv2.VideoWriter(filename, fourcc, FPS, (width, height))
    
    print(f"[info] Saving video to {filename} with {len(buffer)} frames...")
    for frame in buffer:
        out.write(frame)
    out.release()
    print(f"[info] Video saved to: {filename}")

def start_silence_keeper():
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

def audio_callback(indata, frames, time, status):
    if status:
        print(f"[warning] Audio status warning: {status}")
    # The shape of indata is (frames, channels). Directly extending would flatten it.  
    # To maintain the 2D structure, we need to append frame by frame or as a whole,
    # but the fastest way in deque is to convert it to a list and store it.  
    # Alternatively, to avoid flattening, we can use numpy's append, but deque is more efficient:
    audio_buffer.extend(indata.copy())

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

def save_audio_buffer(filename=f"{DEFAULT_OUTPUT_DIR}/crash_audio.wav"):
    if not audio_buffer or len(audio_buffer) == 0:
        print("[warning] Audio buffer is empty, no audio file generated.")
        return
    
    # Repack the data in the deque into a standard 2D numpy array (samples, channels)
    audio_data = np.array(audio_buffer)
    
    # Ensure the data shape is correct (if extend caused shape changes, force reshape)
    if len(audio_data.shape) == 1:
        audio_data = audio_data.reshape(-1, AUDIO_CHANNELS)
        
    # Save as WAV file
    wavfile.write(filename, AUDIO_SAMPLE_RATE, audio_data)
    print(f"[info] Audio successfully saved to: {filename} (total {len(audio_data)/AUDIO_SAMPLE_RATE:.2f} seconds)")

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
    silence_stream = start_silence_keeper()
    print("[info] Pre-recording started, waiting for the game to launch...")
    game_started: bool = False
    
    while True:
        # check if the game process exists and get the window rect
        process_exists = any(p.name() == PROCESS_NAME for p in psutil.process_iter(attrs=['name']))
        
        rect = get_game_window_rect()
        
        if process_exists and rect:
            start_time = time.time()
            if not game_started:
                print("[info] Arknights process detected and window found, starting pre-recording...")
                game_started = True
            
            # if gw.getActiveWindow().title != "明日方舟": continue
            
            # capture the screen
            # mss would capture the screen in BGRA format, we need to convert it to BGR for OpenCV
            sct_img = sct.grab(rect)
            frame = np.array(sct_img)
            frame = cv2.cvtColor(frame, cv2.COLOR_BGRA2BGR) # 转换为 OpenCV 格式
            
            # push to buffer
            frame_buffer.append(frame)
            
            # frame rate control
            elapsed = time.time() - start_time
            if elapsed < FRAME_DELAY:
                time.sleep(FRAME_DELAY - elapsed)
                
        elif len(frame_buffer) > 0:
            # save the crash report when the game process is lost
            print("[info] Game process lost, saving crash report...")
            save_buffer_to_video(frame_buffer)
            save_audio_buffer()
            try_merge_audio_video()
            break
        else:
            time.sleep(1)

if __name__ == "__main__":
    main()