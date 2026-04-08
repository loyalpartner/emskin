;;; eaf-eafvil.el --- Emacs IPC client for the eafvil Wayland compositor  -*- lexical-binding: t; -*-

(require 'json)

;; ---------------------------------------------------------------------------
;; Customization
;; ---------------------------------------------------------------------------

(defgroup eaf-eafvil nil
  "Interface to the eafvil nested Wayland compositor."
  :prefix "eaf-eafvil-"
  :group 'applications)

(defcustom eaf-eafvil-ipc-path nil
  "Explicit IPC socket path.  When nil, auto-discovered via parent PID."
  :type '(choice (const nil) string)
  :group 'eaf-eafvil)

;; ---------------------------------------------------------------------------
;; Internal state
;; ---------------------------------------------------------------------------

(defvar eaf-eafvil--process nil
  "The network process connected to eafvil's IPC socket.")

(defvar eaf-eafvil--read-buf ""
  "Accumulates raw bytes received from eafvil.")

(defvar eaf-eafvil--header-offset nil
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once from compositor-reported surface height.")

(defvar-local eaf-eafvil--window-id nil
  "eafvil window_id for the EAF app embedded in this buffer.")

(defvar-local eaf-eafvil--visible nil
  "Whether this EAF buffer is currently displayed in an Emacs window.")

(defvar eaf-eafvil--displayed-table (make-hash-table :test 'eql)
  "Reusable hash-table for `eaf-eafvil--sync-all' to avoid per-call allocation.")

;; ---------------------------------------------------------------------------
;; Socket discovery
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--ipc-path ()
  "Return the IPC socket path, auto-discovering via parent PID when needed."
  (or eaf-eafvil-ipc-path
      (let* ((ppid (string-trim
                    (shell-command-to-string
                     (format "cat /proc/%d/status | awk '/^PPid:/{print $2}'"
                             (emacs-pid)))))
             (runtime-dir (or (getenv "XDG_RUNTIME_DIR") "/tmp")))
        (format "%s/eafvil-%s.ipc" runtime-dir ppid))))

;; ---------------------------------------------------------------------------
;; Codec: 4-byte u32 LE length prefix + JSON payload
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--encode-message (msg)
  "Encode MSG (alist/plist) as a framed JSON message (unibyte string)."
  (let* ((json (encode-coding-string (json-encode msg) 'utf-8 t))
         (len (length json))
         (prefix (unibyte-string
                  (logand len #xff)
                  (logand (ash len -8) #xff)
                  (logand (ash len -16) #xff)
                  (logand (ash len -24) #xff))))
    (concat prefix json)))

(defun eaf-eafvil--decode-next ()
  "Extract one complete message from `eaf-eafvil--read-buf'.
Returns parsed JSON (hash-table) or nil if more data is needed.
Coerces buffer to unibyte so aref always yields raw byte values 0-255."
  (when (>= (length eaf-eafvil--read-buf) 4)
    (let* ((b0 (aref eaf-eafvil--read-buf 0))
           (b1 (aref eaf-eafvil--read-buf 1))
           (b2 (aref eaf-eafvil--read-buf 2))
           (b3 (aref eaf-eafvil--read-buf 3))
           (len (+ b0 (ash b1 8) (ash b2 16) (ash b3 24))))
      (when (>= (length eaf-eafvil--read-buf) (+ 4 len))
        (let* ((payload (decode-coding-string
                         (substring eaf-eafvil--read-buf 4 (+ 4 len)) 'utf-8))
               (obj (json-parse-string payload)))
          (setq eaf-eafvil--read-buf
                (substring eaf-eafvil--read-buf (+ 4 len)))
          obj)))))

;; ---------------------------------------------------------------------------
;; Process filter (calloop equivalent on the Emacs side)
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--filter (proc data)
  "Accumulate DATA from PROC and dispatch complete messages."
  (ignore proc)
  (setq eaf-eafvil--read-buf
        (concat eaf-eafvil--read-buf (string-as-unibyte data)))
  (let (msg)
    (while (setq msg (eaf-eafvil--decode-next))
      (eaf-eafvil--dispatch msg))))

(defun eaf-eafvil--sentinel (proc event)
  "Handle IPC connection state changes."
  (when (string-match-p "\\(closed\\|failed\\|broken\\|finished\\)" event)
    (message "eafvil: IPC connection %s" (string-trim event))
    (setq eaf-eafvil--process nil)))

;; ---------------------------------------------------------------------------
;; Message dispatch
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--dispatch (msg)
  "Dispatch a parsed MSG hash-table from eafvil."
  (let ((type (gethash "type" msg "")))
    (cond
     ((string= type "connected")
      (message "eafvil: connected (version %s)" (gethash "version" msg "?")))
     ((string= type "error")
      (message "eafvil error: %s" (gethash "msg" msg "")))
     ((string= type "window_created")
      (eaf-eafvil--on-window-created (gethash "window_id" msg)
                                  (gethash "title" msg "")))
     ((string= type "window_destroyed")
      (eaf-eafvil--on-window-destroyed (gethash "window_id" msg)))
     ((string= type "title_changed")
      (eaf-eafvil--on-title-changed (gethash "window_id" msg)
                                 (gethash "title" msg "")))
     ((string= type "surface_size")
      (let* ((h (gethash "height" msg))
             (offset (max 0 (- h (frame-pixel-height)))))
        (setq eaf-eafvil--header-offset offset)
        (message "eafvil: surface=%sx%s bars=%dpx"
                 (gethash "width" msg) h offset)
        ;; Re-sync all EAF windows now that we have the correct offset.
        (dolist (frame (frame-list))
          (eaf-eafvil--sync-all frame))))
     (t
      (message "eafvil: unknown message type %s" type)))))

(defun eaf-eafvil--on-window-created (window-id title)
  "Create/display a buffer for the new EAF app and send initial geometry."
  (let* ((buf-name (format "*eaf: %s*" (if (string-empty-p title) "app" title)))
         (buf (get-buffer-create buf-name)))
    (with-current-buffer buf
      (setq-local eaf-eafvil--window-id window-id)
      (setq-local mode-name "EAF")
      (setq-local buffer-read-only t)
      (eaf-eafvil-input-mode 1)
      (add-hook 'kill-buffer-hook #'eaf-eafvil--kill-buffer-hook nil t))
    (switch-to-buffer buf)
    (when-let ((win (get-buffer-window buf t)))
      (eaf-eafvil--report-geometry window-id win))
    (message "eafvil: EAF app ready (id=%s)" window-id)))

(defun eaf-eafvil--find-buffer (window-id)
  "Return the buffer whose `eaf-eafvil--window-id' equals WINDOW-ID, or nil."
  (seq-find (lambda (buf)
              (equal (buffer-local-value 'eaf-eafvil--window-id buf) window-id))
            (buffer-list)))

(defun eaf-eafvil--on-window-destroyed (window-id)
  "Kill the EAF buffer associated with WINDOW-ID."
  (when-let ((buf (eaf-eafvil--find-buffer window-id)))
    ;; Clear window-id first to prevent kill-buffer-hook from sending
    ;; a redundant "close" message back to the compositor.
    (with-current-buffer buf
      (setq-local eaf-eafvil--window-id nil))
    (kill-buffer buf)
    (message "eafvil: window %s destroyed" window-id)))

(defun eaf-eafvil--on-title-changed (window-id title)
  "Rename the EAF buffer when the app title changes."
  (when-let ((buf (eaf-eafvil--find-buffer window-id)))
    (with-current-buffer buf
      (rename-buffer (format "*eaf: %s*" title) t))))

;; ---------------------------------------------------------------------------
;; Lifecycle: kill-buffer → close
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--kill-buffer-hook ()
  "Notify eafvil to close the app when its Emacs buffer is killed."
  (when eaf-eafvil--window-id
    (eaf-eafvil--send `((type . "close")
                        (window_id . ,eaf-eafvil--window-id)))))

;; ---------------------------------------------------------------------------
;; Key forwarding (Path B): Emacs intercepts keys → forward_key IPC
;; ---------------------------------------------------------------------------

(defconst eaf-eafvil--keycode-table
  (let ((tbl (make-hash-table :test 'equal)))
    ;; Numbers: KEY_1=2 .. KEY_0=11 (same physical keys as QWERTY)
    (puthash ?1 2 tbl) (puthash ?2 3 tbl) (puthash ?3 4 tbl)
    (puthash ?4 5 tbl) (puthash ?5 6 tbl) (puthash ?6 7 tbl)
    (puthash ?7 8 tbl) (puthash ?8 9 tbl) (puthash ?9 10 tbl) (puthash ?0 11 tbl)
    (puthash ?\[ 12 tbl) (puthash ?\] 13 tbl)
    ;; Dvorak top row: ' , . p y f g c r l / =
    (puthash ?' 16 tbl) (puthash ?, 17 tbl) (puthash ?. 18 tbl) (puthash ?p 19 tbl)
    (puthash ?y 20 tbl) (puthash ?f 21 tbl) (puthash ?g 22 tbl) (puthash ?c 23 tbl)
    (puthash ?r 24 tbl) (puthash ?l 25 tbl) (puthash ?/ 26 tbl) (puthash ?= 27 tbl)
    ;; Dvorak home row: a o e u i d h t n s -
    (puthash ?a 30 tbl) (puthash ?o 31 tbl) (puthash ?e 32 tbl) (puthash ?u 33 tbl)
    (puthash ?i 34 tbl) (puthash ?d 35 tbl) (puthash ?h 36 tbl) (puthash ?t 37 tbl)
    (puthash ?n 38 tbl) (puthash ?s 39 tbl) (puthash ?- 40 tbl) (puthash ?` 41 tbl)
    ;; Dvorak bottom row: ; q j k x b m w v z
    (puthash ?\\ 43 tbl)
    (puthash ?\; 44 tbl) (puthash ?q 45 tbl) (puthash ?j 46 tbl) (puthash ?k 47 tbl)
    (puthash ?x 48 tbl) (puthash ?b 49 tbl) (puthash ?m 50 tbl)
    (puthash ?w 51 tbl) (puthash ?v 52 tbl) (puthash ?z 53 tbl)
    ;; Space
    (puthash ?\s 57 tbl)
    ;; Special keys
    (puthash 'escape 1 tbl)
    (puthash 'backspace 14 tbl) (puthash 'tab 15 tbl) (puthash 'return 28 tbl)
    (puthash 'delete 111 tbl) (puthash 'insert 110 tbl)
    ;; Function keys
    (puthash 'f1 59 tbl) (puthash 'f2 60 tbl) (puthash 'f3 61 tbl) (puthash 'f4 62 tbl)
    (puthash 'f5 63 tbl) (puthash 'f6 64 tbl) (puthash 'f7 65 tbl) (puthash 'f8 66 tbl)
    (puthash 'f9 67 tbl) (puthash 'f10 68 tbl) (puthash 'f11 87 tbl) (puthash 'f12 88 tbl)
    ;; Navigation
    (puthash 'up 103 tbl) (puthash 'left 105 tbl)
    (puthash 'right 106 tbl) (puthash 'down 108 tbl)
    (puthash 'home 102 tbl) (puthash 'end 107 tbl)
    (puthash 'prior 104 tbl) (puthash 'next 109 tbl)
    tbl)
  "Map from Emacs event basic-type to Linux evdev keycode.")

(defun eaf-eafvil--event-to-keycode (event)
  "Convert Emacs EVENT to a Linux evdev keycode, or nil if unknown."
  (gethash (event-basic-type event) eaf-eafvil--keycode-table))

(defun eaf-eafvil--forward-key-command ()
  "Forward the current key event to the EAF app in this buffer."
  (interactive)
  (when-let ((wid eaf-eafvil--window-id)
             (event last-input-event)
             (keycode (eaf-eafvil--event-to-keycode event)))
    (let* ((mods (event-modifiers event))
           (mod-mask (logior (if (memq 'control mods) 1 0)
                             (if (memq 'shift mods) 2 0)
                             (if (memq 'meta mods) 4 0))))
      ;; Send press then release (Emacs only sees key-down events).
      (eaf-eafvil--send `((type . "forward_key") (window_id . ,wid)
                          (keycode . ,keycode) (state . 1) (modifiers . ,mod-mask)))
      (eaf-eafvil--send `((type . "forward_key") (window_id . ,wid)
                          (keycode . ,keycode) (state . 0) (modifiers . ,mod-mask))))))

(defvar eaf-eafvil-input-mode-map
  (let ((map (make-sparse-keymap)))
    ;; Printable ASCII (space=32 .. tilde=126)
    (dotimes (i 95)
      (define-key map (vector (+ i 32)) #'eaf-eafvil--forward-key-command))
    ;; Function keys
    (dolist (key '(f1 f2 f3 f4 f5 f6 f7 f8 f9 f10 f11 f12))
      (define-key map (vector key) #'eaf-eafvil--forward-key-command))
    ;; Navigation & editing
    (dolist (key '(left right up down home end prior next
                   backspace delete tab return insert))
      (define-key map (vector key) #'eaf-eafvil--forward-key-command))
    map)
  "Keymap for `eaf-eafvil-input-mode'.
Forwards non-prefix keys to EAF apps.  Emacs prefix keys (C-x, C-c, M-x, C-g, C-h)
are NOT bound here so they pass through to normal Emacs keymaps.")

(define-minor-mode eaf-eafvil-input-mode
  "Minor mode for forwarding keyboard input to EAF app windows."
  :lighter " EAF-Fwd"
  :keymap eaf-eafvil-input-mode-map)

;; ---------------------------------------------------------------------------
;; Public API
;; ---------------------------------------------------------------------------

(defun eaf-eafvil-connect ()
  "Connect to the eafvil IPC socket (auto-discovers path)."
  (interactive)
  (when eaf-eafvil--process
    (delete-process eaf-eafvil--process)
    (setq eaf-eafvil--process nil))
  (setq eaf-eafvil--read-buf "")
  (let ((path (eaf-eafvil--ipc-path)))
    (condition-case err
        (progn
          (setq eaf-eafvil--process
                (make-network-process
                 :name "eaf-eafvil-ipc"
                 :family 'local
                 :service path
                 :coding 'binary
                 :filter #'eaf-eafvil--filter
                 :sentinel #'eaf-eafvil--sentinel
                 :nowait nil))
          (message "eafvil: connecting to %s" path))
      (error
       (message "eafvil: failed to connect to %s: %s" path err)))))

(defun eaf-eafvil--send (msg)
  "Send MSG (alist) to eafvil over IPC."
  (when eaf-eafvil--process
    (process-send-string eaf-eafvil--process (eaf-eafvil--encode-message msg))))

;; ---------------------------------------------------------------------------
;; Geometry reporting
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--frame-header-offset (&optional _frame)
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once when the compositor reports the surface size."
  (or eaf-eafvil--header-offset 0))

(defun eaf-eafvil--window-geometry (window)
  "Return (x y w h) in pixels for Emacs WINDOW.
Coordinates are relative to the top-left of the Wayland surface.
Covers the full window width (including fringes) but excludes the mode-line."
  (let* ((edges (window-pixel-edges window))
         (body-edges (window-body-pixel-edges window))
         (x (nth 0 edges))
         (raw-y (nth 1 edges))
         (y (+ raw-y (eaf-eafvil--frame-header-offset (window-frame window))))
         (w (- (nth 2 edges) x))
         ;; body-bottom = top of mode-line; stop there so mode-line stays visible.
         (h (- (nth 3 body-edges) raw-y)))
    (list x y w h)))

(defun eaf-eafvil-debug-geometry ()
  "Print geometry debug info to *Messages*."
  (interactive)
  (let* ((frame (selected-frame))
         (geom (frame-geometry frame))
         (win (selected-window))
         (root-edges (window-pixel-edges (frame-root-window frame)))
         (mb-h (or (cdr (alist-get 'menu-bar-size geom)) 0))
         (tb-h (or (cdr (alist-get 'tool-bar-size geom)) 0))
         (mb-ext (alist-get 'menu-bar-external geom))
         (tb-ext (alist-get 'tool-bar-external geom))
         (outer-h (cdr (alist-get 'outer-size geom)))
         (pixel-h (frame-pixel-height frame))
         (inner-h (frame-inner-height frame))
         (mb-lines (frame-parameter frame 'menu-bar-lines))
         (offset (eaf-eafvil--frame-header-offset frame))
         (final (eaf-eafvil--window-geometry win)))
    (message (concat "eafvil-debug: "
                     "mb: h=%d ext=%s lines=%s | "
                     "tb: h=%d ext=%s | "
                     "outer-h=%s pixel-h=%d inner-h=%d | "
                     "root-edges: %s | "
                     "offset: %d | final: %s")
             mb-h mb-ext mb-lines
             tb-h tb-ext
             outer-h pixel-h inner-h
             root-edges offset final)))

(defvar-local eaf-eafvil--last-geometry nil
  "Last geometry sent for this buffer's EAF window, to skip no-op updates.")

(defun eaf-eafvil--report-geometry (window-id window)
  "Send set_geometry for WINDOW-ID, only when geometry actually changed."
  (let ((geo (eaf-eafvil--window-geometry window)))
    (unless (equal geo (buffer-local-value 'eaf-eafvil--last-geometry
                                           (window-buffer window)))
      (with-current-buffer (window-buffer window)
        (setq-local eaf-eafvil--last-geometry geo))
      (eaf-eafvil--send `((type . "set_geometry")
                      (window_id . ,window-id)
                      (x . ,(nth 0 geo))
                      (y . ,(nth 1 geo))
                      (w . ,(nth 2 geo))
                      (h . ,(nth 3 geo)))))))

(defun eaf-eafvil--sync-all (_frame)
  "Sync visibility and geometry for all EAF buffers across all frames."
  (let ((displayed eaf-eafvil--displayed-table))
    (clrhash displayed)
    ;; Pass 1: collect currently displayed EAF window-ids.
    ;; When the same EAF buffer is shown in multiple windows (e.g. C-x 3),
    ;; prefer selected-window so the app surface follows the active window.
    (dolist (fr (frame-list))
      (dolist (win (window-list fr 'no-minibuf))
        (when-let ((wid (buffer-local-value 'eaf-eafvil--window-id
                                            (window-buffer win))))
          (when (or (not (gethash wid displayed))
                    (eq win (selected-window)))
            (puthash wid win displayed)))))
    ;; Pass 2: update visibility and geometry for every EAF buffer.
    (dolist (buf (buffer-list))
      (when-let ((wid (buffer-local-value 'eaf-eafvil--window-id buf)))
        (let* ((win (gethash wid displayed))
               (now-visible (and win t))
               (was-visible (buffer-local-value 'eaf-eafvil--visible buf)))
          ;; Send set_visibility only when state changed.
          (unless (eq now-visible was-visible)
            (with-current-buffer buf
              (setq-local eaf-eafvil--visible now-visible))
            (eaf-eafvil--send `((type . "set_visibility")
                                (window_id . ,wid)
                                (visible . ,(if now-visible t :false)))))
          ;; Sync geometry for visible windows.
          (when win
            (eaf-eafvil--report-geometry wid win)))))))

(add-hook 'window-size-change-functions #'eaf-eafvil--sync-all)
(add-hook 'window-buffer-change-functions #'eaf-eafvil--sync-all)

;; ---------------------------------------------------------------------------
;; Launch an EAF application
;; ---------------------------------------------------------------------------

(defcustom eaf-eafvil-demo-dir
  (expand-file-name
   "../demo"
   (file-name-directory
    (or load-file-name buffer-file-name
        "~/.emacs.d/site-lisp/emacs-application-framework/mvp/elisp/")))
  "Directory containing EAF demo/app Python scripts."
  :type 'directory
  :group 'eaf-eafvil)

(defun eaf-open-app (app-name)
  "Launch EAF application APP-NAME (Python script in `eaf-eafvil-demo-dir')."
  (interactive "sApp name: ")
  (let* ((script (expand-file-name (format "%s.py" app-name) eaf-eafvil-demo-dir))
         (process-environment
          (cons (format "WAYLAND_DISPLAY=%s" (or (getenv "WAYLAND_DISPLAY") ""))
                process-environment)))
    (unless (file-exists-p script)
      (error "EAF script not found: %s" script))
    (start-process (format "eaf-%s" app-name) nil "python3" script)
    (message "eafvil: launched %s" app-name)))

;; ---------------------------------------------------------------------------
;; Auto-connect when running inside eafvil
;; ---------------------------------------------------------------------------

(defun eaf-eafvil-maybe-auto-connect ()
  "Connect to eafvil IPC if we appear to be running inside eafvil.
Checks for the eaf-eafvil-specific socket file derived from our parent PID."
  (when (featurep 'pgtk)
    (let ((path (eaf-eafvil--ipc-path)))
      (when (file-exists-p path)
        (run-with-timer 0.5 nil #'eaf-eafvil-connect)))))

;; Hook into Emacs startup.
(add-hook 'emacs-startup-hook #'eaf-eafvil-maybe-auto-connect)

(provide 'eaf-eafvil)
;;; eaf-eafvil.el ends here
