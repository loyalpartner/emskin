;;; emskin-jelly.el --- Jelly text-cursor animation  -*- lexical-binding: t; -*-

;; Algorithm ported from holo-layer's `holo-layer-get-cursor-info' — pure
;; elisp + IPC, no Wayland `text_input_v3' dependency (pgtk doesn't
;; broadcast caret position on that channel reliably when an IM like
;; fcitx intercepts the GTK IM context).

;;; Code:

(require 'emskin-ipc)
(require 'emskin-app)  ; emskin--frame-header-offset

(defvar emskin-jelly-cursor)
(defvar emskin-jelly-fallback-color)

(defvar emskin--jelly-last-info nil
  "Last sent jelly caret `(WINDOW . \"x:y:w:h:color\")' pair.
The window component is a cheap tiebreaker so moving to a different
window at the same pixel coords still fires a new animation.")

;; ---------------------------------------------------------------------------
;; Caret rect computation
;; ---------------------------------------------------------------------------

(defun emskin--jelly-window-origin (window)
  "Return (X Y) of WINDOW's top-left in Emacs surface coordinates."
  (let* ((frame (window-frame window))
         (edges (window-pixel-edges window))
         (header (emskin--frame-header-offset frame))
         ;; Child frame offset relative to the root (parent) frame surface.
         (frame-x (or (frame-parameter frame 'left) 0))
         (frame-y (or (frame-parameter frame 'top) 0)))
    (list (+ (nth 0 edges) frame-x)
          (+ (nth 1 edges) header frame-y))))

(defun emskin--jelly-cursor-rect ()
  "Return (X Y W H COLOR) of the text caret in surface pixels, or nil."
  (when-let* ((p (point))
              (window (selected-window))
              (vis (pos-visible-in-window-p p window t))
              (alloc (emskin--jelly-window-origin window)))
    (let* ((wx (nth 0 alloc))
           (wy (nth 1 alloc))
           (fringe-l (or (car (window-fringes window)) 0))
           (margin-l (or (car (window-margins window)) 0))
           (cw (frame-char-width))
           (cursor-w (if (eq cursor-type 'bar) 1 cw))
           (cursor-h (line-pixel-height))
           (x (+ (nth 0 vis) wx fringe-l (* margin-l cw)))
           (y (+ (nth 1 vis) wy))
           (color (or (face-background 'cursor nil t)
                      emskin-jelly-fallback-color)))
      (list x y cursor-w cursor-h color))))

;; ---------------------------------------------------------------------------
;; Post-command monitor
;; ---------------------------------------------------------------------------

(defun emskin--jelly-send (info)
  "Send caret INFO (x y w h color) or nil (cancel) to the compositor."
  (emskin--send `((type . "set_cursor_rect")
                  (x . ,(or (nth 0 info) 0))
                  (y . ,(or (nth 1 info) 0))
                  (w . ,(or (nth 2 info) 0))
                  (h . ,(or (nth 3 info) 0))
                  (color . ,(or (nth 4 info) :null)))))

(defun emskin--jelly-monitor ()
  "Push the current caret rect if it moved.
Self-gates on `emskin-jelly-cursor' and `emskin--process' so the hook
can stay permanently installed on `post-command-hook'."
  (when (and emskin-jelly-cursor emskin--process)
    (let ((info (emskin--jelly-cursor-rect))
          (window (selected-window)))
      (cond
       ((null info)
        (when emskin--jelly-last-info
          (emskin--jelly-send nil)
          (setq emskin--jelly-last-info nil)))
       (t
        (let ((key (cons window
                         (format "%d:%d:%d:%d:%s"
                                 (nth 0 info) (nth 1 info)
                                 (nth 2 info) (nth 3 info) (nth 4 info)))))
          (unless (equal key emskin--jelly-last-info)
            (emskin--jelly-send info)
            (setq emskin--jelly-last-info key))))))))

(defun emskin--jelly-cursor-sync ()
  (unless emskin-jelly-cursor
    ;; Clear the dedup key so the next enable re-primes from the real
    ;; current caret position instead of animating across the gap.
    (setq emskin--jelly-last-info nil))
  (emskin--send `((type . "set_jelly_cursor")
                  (enabled . ,(emskin--jbool emskin-jelly-cursor)))))

(emskin-define-toggle jelly-cursor "jelly cursor")

(add-hook 'post-command-hook #'emskin--jelly-monitor)

(provide 'emskin-jelly)
;;; emskin-jelly.el ends here
