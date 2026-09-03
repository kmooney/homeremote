# Home Remote

This is a project, built in rust to create a remote for my home.

The problem: I have a samsung TV, Denon reciever, and an apple TV.

These are the 3 devices I use most to watch TV. I currently use a logitech
harmony remote. It's terrible. The IR repeaters work inconsistently, so I cannot
consistently control the volume when I am watching Apple TV.

All of these devices are currently on my network, and are IP controllable.

I want to make two things: 1) a rust library that lets me control all of the
devices, via IP, on the network. It should scan for the devices on the network
so I do not have to set the IP address manually.

The Denon is a AVR-S720W - the current Denon AVR app on my iPhone can detect and
control the app. I want to be able to control the volume, and pick the correct
input.

The Apple TV should be controllable as if from the Harmony app. I believe the
harmony app still uses IP control for the apple TV.

The Samsung is a Samsung Smart TV.

Structure:

1. Build a rust library crate to control the TV
2. Build a rust daemon that i can access via a react app that will let me
   control the receiver, the TV and apple TV from a single application.
