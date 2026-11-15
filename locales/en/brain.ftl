callback-ok = Sign-in response received. You can close this tab and return to the app.
callback-rejected = This sign-in response was rejected. Return to the app and try again.
callback-failed = Sign-in failed. Return to the app and start sign-in again.

# A turn asking about a clicked 3D part. Each value is drawing text the model wrote, quoted as
# description, never as instructions.
ask-about-part = Tell me about this part of the drawing: “{ $label }”.
ask-about-part-info = Its description: “{ $info }”.
ask-about-part-alt = The drawing shows: “{ $alt }”.

# A held turn's screen, for the model. Fixed text only, never window details.
screen-snapshot = The image is a snapshot from when the user spoke.
screen-not-ready = The screen was not included: it wasn't ready in time.
screen-nothing-there = The screen was not included: there was no window under the pointer.
screen-cant-share = The screen was not included: the window can't be shared.
screen-unreadable = The screen was not included: the window under the pointer couldn't be told apart from one drawn over it, so no picture was sent.
screen-too-large = The screen was not included: the window image was too large.
screen-changed = The screen was not included: the window changed while it was being captured.
screen-model-cant-see = The screen was not included: this model can't be sent images.
# A screen hold's own lines. The rectangle is the painted line's bounds, in image pixels.
screen-picture = The image is a picture of the user's screen, taken when they let go of the talk keys.
screen-pointed = While speaking, the user moved the mouse pointer to show you something. The pink line on the image follows that movement. It was drawn on the image for you and is not on their screen. It lies inside the rectangle from ({ $left }, { $top }) to ({ $right }, { $bottom }), in image pixels.
screen-picture-too-large = The screen was not included: its picture was too large.
screen-display-changed = The screen was not included: the display changed while it was being captured.
