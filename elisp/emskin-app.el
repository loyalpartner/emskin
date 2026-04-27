;;; emskin-app.el --- Embedded app lifecycle, geometry, and mirrors  -*- lexical-binding: t; -*-

(require 'emskin-ipc)

;; ---------------------------------------------------------------------------
;; Message dispatch
;; ---------------------------------------------------------------------------

(defun emskin--dispatch (msg)
  "Dispatch a parsed MSG hash-table from emskin."
  (let ((type (gethash "type" msg "")))
    (cond
     ((string= type "connected")
      (message "emskin: connected (version %s)" (gethash "version" msg "?"))
      ;; Initial frame = workspace 1.
      (setq emskin--active-workspace-id 1)
      (puthash (selected-frame) 1 emskin--frame-workspace-table)
      (run-hooks 'emskin-connected-hook))
     ((string= type "error")
      (message "emskin error: %s" (gethash "msg" msg "")))
     ((string= type "window_created")
      (emskin--on-window-created (gethash "window_id" msg)
                                  (gethash "title" msg "")))
     ((string= type "window_destroyed")
      (emskin--on-window-destroyed (gethash "window_id" msg)))
     ((string= type "title_changed")
      (emskin--on-title-changed (gethash "window_id" msg)
                                 (gethash "title" msg "")))
     ((string= type "focus_view")
      (emskin--on-focus-view (gethash "window_id" msg)
                                 (gethash "view_id" msg)))
     ((string= type "surface_size")
      (let* ((w (gethash "width" msg))
             (h (gethash "height" msg))
             (frame-h (frame-pixel-height))
             ;; Only seed header-offset once — it's the GTK external
             ;; menu-bar / tool-bar height, which is a property of the
             ;; Emacs frame, NOT of the compositor's surface. Recomputing
             ;; from `h - frame-pixel-height` on every resize races with
             ;; GTK's own resize processing: a compositor bar appearing /
             ;; disappearing (C-x 5 1 / 5 2) would briefly make frame-h
             ;; stale and shift apps by the bar height.
             (offset (or emskin--header-offset
                         (max 0 (- h frame-h)))))
        (setq emskin--header-offset offset)
        (message "emskin: surface=%sx%s bars=%dpx" w h offset)
        ;; Re-sync EAF windows with the updated surface dims.
        (dolist (frame (frame-list))
          (emskin--sync-frame frame))))
     ((string= type "workspace_created")
      (emskin--on-workspace-created (gethash "workspace_id" msg)))
     ((string= type "workspace_switched")
      (emskin--on-workspace-switched (gethash "workspace_id" msg)))
     ((string= type "workspace_destroyed")
      (emskin--on-workspace-destroyed (gethash "workspace_id" msg)))
     ((string= type "recording_stopped")
      (emskin--on-recording-stopped (gethash "path" msg "")
                                    (gethash "frames_written" msg 0)
                                    (gethash "duration_secs" msg 0.0)
                                    (gethash "reason" msg "unknown")))
     (t
      (message "emskin: unknown message type %s" type)))))

(defun emskin--on-recording-stopped (path frames duration reason)
  "Handle a `recording_stopped' IPC: sync `emskin-record' and notify user.

The compositor may auto-stop (resize / encoder error / replaced) without
Emacs having asked, so we unconditionally flip `emskin-record' off here
rather than assuming the elisp toggle is the single source of truth."
  (when (boundp 'emskin-record)
    (setq emskin-record nil))
  (let ((tag (pcase reason
               ("user" "done")
               ("resize" "stopped (screen size changed)")
               ("encoder_error" "stopped (encoder error)")
               ("replaced" "stopped (superseded)")
               (_ (format "stopped (%s)" reason)))))
    (message "emskin: recording %s — %d frames, %.2fs → %s"
             tag frames duration path)))

;; ---------------------------------------------------------------------------
;; Window lifecycle
;; ---------------------------------------------------------------------------

(defun emskin--on-window-created (window-id title)
  "Create/display a buffer for the new embedded app and send initial geometry."
  ;; `generate-new-buffer' guarantees a fresh buffer even when titles
  ;; collide (two xterms, two firefox windows, …). `get-buffer-create'
  ;; would return the existing buffer and silently overwrite its
  ;; `emskin--window-id', losing the mapping for the earlier window.
  (let* ((buf-name (format "*emskin: %s*" (if (string-empty-p title) "app" title)))
         (buf (generate-new-buffer buf-name)))
    (with-current-buffer buf
      (setq-local emskin--window-id window-id)
      (setq-local mode-name "emskin")
      (setq-local buffer-read-only t)
      (setq-local left-fringe-width 0)
      (setq-local right-fringe-width 0)
      (setq-local left-margin-width 0)
      (setq-local right-margin-width 0)
      (setq-local cursor-type nil)
      (add-hook 'kill-buffer-hook #'emskin--kill-buffer-hook nil t)
      ;; Buffer-local: only fires when the user is STILL on this EAF
      ;; buffer after the prefix command. That's the right time to ask
      ;; the compositor to restore keyboard focus to the embedded app.
      ;; If the prefix command (e.g. C-x b) switches to a different
      ;; buffer, post-command-hook fires for THAT buffer instead — the
      ;; global `prefix_clear' hook below picks up the IME-state-only
      ;; cleanup without bouncing focus back to the now-hidden EAF.
      (add-hook 'post-command-hook #'emskin--post-command-prefix-done nil t))
    (let ((target (emskin--take-native-app-target-window)))
      (if target
          (set-window-buffer target buf)
        (display-buffer buf '((display-buffer-pop-up-window
                               display-buffer-use-some-window)
                              (inhibit-same-window . t)
                              (reusable-frames . nil)))))
    (when-let ((win (get-buffer-window buf t)))
      (set-window-scroll-bars win 0 nil 0 nil)
      (emskin--report-geometry window-id win))
	(emskin--sync-focus)
    (message "emskin: embedded app ready (id=%s)" window-id)))

(defun emskin--find-buffer (window-id)
  "Return the buffer whose `emskin--window-id' equals WINDOW-ID, or nil."
  (seq-find (lambda (buf)
              (equal (buffer-local-value 'emskin--window-id buf) window-id))
            (buffer-list)))

(defun emskin--on-window-destroyed (window-id)
  "Close all Emacs windows/buffer for WINDOW-ID and restore focus."
  (when-let ((buf (emskin--find-buffer window-id)))
    ;; Clear window-id first to prevent kill-buffer-hook from sending
    ;; a redundant "close" message back to the compositor.
    (with-current-buffer buf
      (setq-local emskin--window-id nil))
    ;; Delete ALL windows showing this buffer (source + mirrors).
    ;; Walk in reverse so deletion doesn't invalidate the list.
    (dolist (win (reverse (get-buffer-window-list buf nil t)))
      ;; `window-list` count is not enough here: the main window of a frame
      ;; can still be non-deletable when side windows exist. Guard with the
      ;; real Emacs predicate to avoid "Attempt to delete main window".
      (when (window-deletable-p win)
        (delete-window win)))
    (kill-buffer buf)
    ;; Clean up mirror-table entry.
    (remhash window-id emskin--mirror-table)
    ;; After window/buffer removal, check if the now-selected buffer is
    ;; an emskin app and send set_focus so the compositor matches.
    (let ((next-wid (buffer-local-value 'emskin--window-id
                                        (window-buffer (selected-window)))))
      (emskin--send (if next-wid
                        `((type . "set_focus") (window_id . ,next-wid))
                      '((type . "set_focus")))))
    (message "emskin: window %s destroyed" window-id)))

(defun emskin--on-title-changed (window-id title)
  "Rename the EAF buffer when the app title changes."
  (when-let ((buf (emskin--find-buffer window-id)))
    (with-current-buffer buf
      (rename-buffer (format "*emskin: %s*" title) t))))

(defun emskin--on-focus-view (window-id view-id)
  "Select the Emacs window that corresponds to WINDOW-ID / VIEW-ID.
VIEW-ID 0 means the source window; otherwise look up the mirror alist."
  (let* ((state (gethash window-id emskin--mirror-table))
         (target (when state
                   (if (= view-id 0)
                       (car state)
                     (cdr (assq view-id (cdr state)))))))
    ;; Fallback: search current frame only to avoid cross-workspace switch.
    (unless (and target (window-live-p target)
                 (eq (window-frame target) (selected-frame)))
      (when-let ((buf (emskin--find-buffer window-id)))
        ;; nil = search current frame only (not t = all frames).
        (setq target (get-buffer-window buf nil))))
    (when (and target (window-live-p target))
      (select-window target))))

(defun emskin--kill-buffer-hook ()
  "Notify emskin to close the app when its Emacs buffer is killed."
  (when emskin--window-id
    (emskin--send `((type . "close")
                        (window_id . ,emskin--window-id)))))

(defun emskin--post-command-prefix-done ()
  "After a command completes IN AN EAF BUFFER, ask the compositor to
restore keyboard focus to the embedded app (clearing prefix state
along the way). Registered buffer-locally — only fires when the
post-command tick runs while the EAF buffer is still current."
  (when emskin--process
    (emskin--send '((type . "prefix_done")))))

(defun emskin--post-command-prefix-clear ()
  "Clear the compositor's `prefix_active' flag after every Emacs
command, in any buffer.

emskin disables host IME for the duration of an Emacs prefix chord
(C-x ...) so the chord doesn't get eaten by fcitx5. Without a global
clear, plain Emacs chords like `C-x b' (which switch to a non-EAF
buffer, so the buffer-local `prefix_done' hook above doesn't fire)
would leave host IME disabled until the user clicks out and back.

Unlike `prefix_done', this signal does NOT restore focus — focus
follows whatever Emacs's prefix command did. The IPC handler is a
no-op when no prefix is active, so per-command firing is cheap."
  (when emskin--process
    (emskin--send '((type . "prefix_clear")))))

;; Global registration of the IME-only cleanup. Idempotent.
(add-hook 'post-command-hook #'emskin--post-command-prefix-clear)

;; ---------------------------------------------------------------------------
;; Geometry reporting
;; ---------------------------------------------------------------------------

(defun emskin--frame-header-offset (&optional _frame)
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once when the compositor reports the surface size."
  (or emskin--header-offset 0))

(defun emskin--window-geometry (window)
  "Return (x y w h) in pixels for Emacs WINDOW.
Coordinates are relative to the top-left of the Wayland surface.
Covers the body area (excludes fringes, margins, header-line, mode-line)."
  (let* ((body (window-body-pixel-edges window))
         (off (emskin--frame-header-offset (window-frame window)))
         (x (nth 0 body))
         (raw-y (nth 1 body))
         (y (+ raw-y off))
         (w (- (nth 2 body) x))
         (h (- (nth 3 body) raw-y)))
    (list x y w h)))

(defun emskin-debug-geometry ()
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
         (offset (emskin--frame-header-offset frame))
         (final (emskin--window-geometry win)))
    (message (concat "emskin-debug: "
                     "mb: h=%d ext=%s lines=%s | "
                     "tb: h=%d ext=%s | "
                     "outer-h=%s pixel-h=%d inner-h=%d | "
                     "root-edges: %s | "
                     "offset: %d | final: %s")
             mb-h mb-ext mb-lines
             tb-h tb-ext
             outer-h pixel-h inner-h
             root-edges offset final)))

(defun emskin--report-geometry (window-id window)
  "Send set_geometry for WINDOW-ID, only when geometry actually changed."
  (let ((geo (emskin--window-geometry window)))
    (unless (equal geo (buffer-local-value 'emskin--last-geometry
                                           (window-buffer window)))
      (with-current-buffer (window-buffer window)
        (setq-local emskin--last-geometry geo))
      (emskin--send `((type . "set_geometry")
                      (window_id . ,window-id)
                      (x . ,(nth 0 geo))
                      (y . ,(nth 1 geo))
                      (w . ,(nth 2 geo))
                      (h . ,(nth 3 geo)))))))

(defun emskin--alloc-view-id ()
  "Allocate a unique mirror view ID."
  (cl-incf emskin--next-view-id))

(defun emskin--send-mirror-geometry (wid view-id win msg-type)
  "Send mirror geometry IPC for WID/VIEW-ID at Emacs WIN position."
  (let ((geo (emskin--window-geometry win)))
    (emskin--send `((type . ,msg-type)
                    (window_id . ,wid)
                    (view_id . ,view-id)
                    (x . ,(nth 0 geo))
                    (y . ,(nth 1 geo))
                    (w . ,(nth 2 geo))
                    (h . ,(nth 3 geo))))))

;; ---------------------------------------------------------------------------
;; Per-frame sync (geometry + visibility + mirrors)
;; ---------------------------------------------------------------------------

(defun emskin--sync-frame (frame)
  "Sync visibility, geometry, and mirrors for EAF buffers in FRAME.
Only processes FRAME — never iterates other frames.  Skips the sync
if FRAME does not belong to the active workspace, which eliminates
race conditions during workspace switches."
  (when emskin--process
  (let ((ws-id (gethash frame emskin--frame-workspace-table)))
    (when (eql ws-id emskin--active-workspace-id)
    ;; Pass 1: collect Emacs windows showing each EAF buffer in this frame.
    (let ((wid-wins (make-hash-table :test 'eql)))
      (dolist (win (window-list frame 'no-minibuf))
        (when-let ((wid (buffer-local-value 'emskin--window-id
                                            (window-buffer win))))
          (set-window-scroll-bars win 0 nil 0 nil)
          (set-window-fringes win 0 0)
          (set-window-margins win 0 0)
          (puthash wid (append (gethash wid wid-wins) (list win)) wid-wins)))
      ;; Pass 2: for each EAF buffer, sync source + mirrors.
      (dolist (buf (buffer-list))
        (when-let ((wid (buffer-local-value 'emskin--window-id buf)))
          (let* ((wins (gethash wid wid-wins))
                 (now-visible (and wins t))
                 (was-visible (buffer-local-value 'emskin--visible buf))
                 (prev-state (gethash wid emskin--mirror-table))
                 (prev-source (car prev-state))
                 (prev-mirrors (cdr prev-state)))
            ;; Visibility change.
            (unless (eq now-visible was-visible)
              (with-current-buffer buf
                (setq-local emskin--visible now-visible))
              (emskin--send `((type . "set_visibility")
                              (window_id . ,wid)
                              (visible . ,(if now-visible t :json-false)))))
            (if (not wins)
                ;; No windows showing this buffer — clean up mirrors.
                (progn
                  (dolist (m prev-mirrors)
                    (emskin--send `((type . "remove_mirror")
                                    (window_id . ,wid)
                                    (view_id . ,(car m)))))
                  (remhash wid emskin--mirror-table))
              ;; Determine source window: keep prev-source if still showing,
              ;; otherwise use first window in the list.
              (let* ((source-win (if (and prev-source (memq prev-source wins))
                                     prev-source
                                   (car wins)))
                     (mirror-wins (remq source-win wins))
                     (new-mirrors nil))
                ;; Source changed — remove all old mirrors and rebuild.
                (when (and prev-source (not (eq source-win prev-source)))
                  (dolist (m prev-mirrors)
                    (emskin--send `((type . "remove_mirror")
                                    (window_id . ,wid)
                                    (view_id . ,(car m)))))
                  (setq prev-mirrors nil))
                ;; Sync source geometry.
                (emskin--report-geometry wid source-win)
                ;; Reconcile mirrors.
                (let ((old-by-win (make-hash-table :test 'eq)))
                  (dolist (m prev-mirrors)
                    (puthash (cdr m) (car m) old-by-win))
                  (dolist (mw mirror-wins)
                    (let ((vid (or (gethash mw old-by-win)
                                   (emskin--alloc-view-id))))
                      (push (cons vid mw) new-mirrors)
                      (if (gethash mw old-by-win)
                          (emskin--send-mirror-geometry
                           wid vid mw "update_mirror_geometry")
                        (emskin--send-mirror-geometry
                         wid vid mw "add_mirror"))
                      (remhash mw old-by-win)))
                  (maphash (lambda (_win vid)
                             (emskin--send `((type . "remove_mirror")
                                             (window_id . ,wid)
                                             (view_id . ,vid))))
                           old-by-win))
                (puthash wid (cons source-win (nreverse new-mirrors))
                         emskin--mirror-table)))))))))))

(add-hook 'window-size-change-functions #'emskin--sync-frame)
(add-hook 'window-buffer-change-functions #'emskin--sync-frame)

;; ---------------------------------------------------------------------------
;; Focus sync
;; ---------------------------------------------------------------------------

(defun emskin--sync-focus (&optional _frame)
  "Tell the compositor which surface should have keyboard focus.
When the selected window shows an EAF buffer, focus the app;
otherwise focus Emacs.  Skips IPC when focus hasn't changed."
  (when emskin--process
    (let ((wid (buffer-local-value 'emskin--window-id
                                   (window-buffer (selected-window)))))
      (unless (eq wid emskin--last-focused-wid)
        (setq emskin--last-focused-wid wid)
        (emskin--send (if wid
                          `((type . "set_focus") (window_id . ,wid))
                        '((type . "set_focus"))))))))

(add-hook 'window-selection-change-functions #'emskin--sync-focus)

(provide 'emskin-app)
;;; emskin-app.el ends here
