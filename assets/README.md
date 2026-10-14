# Сторонние файлы

## gigaam_v3.onnx

Препроцессор GigaAM v3: считает лог-мел-признаки из сырых сэмплов 16 кГц.
Взят из пакета [onnx-asr](https://github.com/istupakov/onnx-asr)
(`src/onnx_asr/preprocessors/data/gigaam_v3.onnx`), лицензия MIT,
автор Илья Ступаков.

Лежит в репозитории, а не качается вместе с моделью, по двум причинам:
весит 44 КБ, и в репозитории самой модели на HuggingFace его нет.
Свой расчёт мел-фильтров здесь был бы лишним риском — признаки должны
совпадать с теми, на которых модель обучалась, до последнего бита.

## Алгоритм декодирования GigaAM RNN-T

Имена тензоров, размеры состояния и алгоритм жадного декодирования в
`src/gigaam.rs` сверены с [onnx-asr](https://github.com/istupakov/onnx-asr):
`src/onnx_asr/models/gigaam.py` и `src/onnx_asr/asr.py`.
Лицензия MIT сохранена в `onnx-asr-LICENSE.txt`.
