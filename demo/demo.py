#!/usr/bin/env python3
"""Minimal EAF demo app — connects to the eafvil Wayland compositor."""
import sys

from PyQt6.QtWidgets import (
    QApplication,
    QLabel,
    QLineEdit,
    QMainWindow,
    QPushButton,
    QTextEdit,
    QVBoxLayout,
    QWidget,
)


class DemoWindow(QMainWindow):
    def __init__(self) -> None:
        super().__init__()
        self.setWindowTitle("EAF Demo")
        self.setStyleSheet("background-color: #2b2b2b; color: white;")

        central = QWidget()
        layout = QVBoxLayout(central)

        layout.addWidget(
            QLabel("<h2>EAF Demo</h2><p>Running inside eafvil compositor.</p>")
        )

        self.input = QLineEdit()
        self.input.setPlaceholderText("Type here to test keyboard input...")
        self.input.setStyleSheet(
            "background: #3c3c3c; color: white; padding: 8px; border: 1px solid #555;"
        )
        layout.addWidget(self.input)

        self.textarea = QTextEdit()
        self.textarea.setPlaceholderText("Multi-line text area...")
        self.textarea.setStyleSheet(
            "background: #3c3c3c; color: white; padding: 8px; border: 1px solid #555;"
        )
        layout.addWidget(self.textarea)

        self.status = QLabel("Status: waiting for input")
        layout.addWidget(self.status)

        btn = QPushButton("Click Me")
        btn.setStyleSheet(
            "background: #4a9eff; color: white; padding: 8px; border: none;"
        )
        btn.clicked.connect(lambda: self.status.setText("Button clicked!"))
        layout.addWidget(btn)

        self.setCentralWidget(central)

    def keyPressEvent(self, event: "QKeyEvent") -> None:  # noqa: N802
        self.status.setText(
            f"Key: {event.text()!r}  scancode={event.nativeScanCode()}"
        )
        super().keyPressEvent(event)


def main() -> None:
    app = QApplication(sys.argv)
    app.setApplicationName("eaf-demo")
    window = DemoWindow()
    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
